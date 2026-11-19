//! Native Desktop lifecycle operations (`Create`, `Destroy`, `Reorder`, `Swap`).

use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use objc2_core_graphics::CGDirectDisplayID;
use tracing::{debug, warn};

use super::ops::{
    DesktopOpWorld, active_display_id, preferred_focus_on_space, resolve_desktop_target,
    save_active_strip_focus, sibling_space_ids,
};
use super::{ConfirmCondition, PendingDesktopOp, PostConfirmAction, refresh_census_from_bridge};
use crate::commands::DesktopTarget;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::{SelectedVirtualMarker, SpawnCommandsExt as _};
use crate::manager::spaces::{DesktopBridge, DisplaySpaces, SpaceCensus, SpaceId, SpaceRecord};
use crate::platform::WinID;

/// Creates a new ordinary Desktop (`type == 0`) via `WMBridge` (§5.4), optionally
/// switching to it once confirmed in the `SkyLight` census.
pub(super) fn handle_create(focus: bool, bridge: &mut DesktopBridge, world: &mut DesktopOpWorld) {
    let Ok(before) = bridge
        .read_census()
        .inspect_err(|err| warn!("desktop create census failed: {err}"))
    else {
        return;
    };
    let new_id = match bridge.create_desktop() {
        Ok(id) => id,
        Err(err) => {
            warn!("desktop create failed: {err}");
            if err.request_may_have_applied {
                refresh_census_from_bridge(bridge, &mut world.census);
            }
            return;
        }
    };
    if before.find_space(new_id).is_some() {
        warn!("desktop create returned pre-existing space {new_id}; resyncing census");
        refresh_census_from_bridge(bridge, &mut world.census);
        return;
    }
    if focus {
        let focused_entity = world.windows.focused().map(|(_, e)| e);
        save_active_strip_focus(
            &world.strips,
            focused_entity,
            &world.windows,
            world.focus_history.as_deref_mut(),
        );
    }
    world.pending.push(PendingDesktopOp::new(
        format!("create desktop {new_id}"),
        ConfirmCondition::SpacePresent(new_id),
        Some(PostConfirmAction::EnsureStripAndMaybeFocus {
            space_id: new_id,
            focus,
        }),
    ));
}

fn adjacent_desktop(disp: &DisplaySpaces, target_id: SpaceId) -> Option<&SpaceRecord> {
    let idx = disp.spaces.iter().position(|s| s.id == target_id)?;
    disp.spaces[..idx]
        .iter()
        .rfind(|s| s.kind.is_desktop())
        .or_else(|| disp.spaces[idx + 1..].iter().find(|s| s.kind.is_desktop()))
}

/// Inspects application windows on `space_id` (`0x7` including minimized, §5.5).
/// Fails closed if any window's `CGWindowList` metadata is unreadable.
fn inspect_space_windows_for_destroy(
    space_id: SpaceId,
    empty_only: bool,
    bridge: &DesktopBridge,
) -> Result<Vec<WinID>, String> {
    let raw_ids = bridge
        .windows_in_space(space_id, true)
        .map_err(|e| format!("enumerating windows in space {space_id}: {e}"))?;
    let mut app_windows = Vec::new();
    let mut has_non_sticky = false;
    for wid in raw_ids {
        let Some(meta) = bridge.window_meta(wid) else {
            return Err(format!(
                "missing WindowServer metadata for window {wid} on space {space_id}"
            ));
        };
        if !meta.is_app_window() {
            continue;
        }
        if bridge.window_is_sticky(wid) != Some(true) {
            has_non_sticky = true;
        }
        app_windows.push(wid);
    }
    if empty_only && has_non_sticky {
        return Err(format!(
            "desktop {space_id} is not empty; refusing --empty-only destroy"
        ));
    }
    Ok(app_windows)
}

/// Drains all tiled columns from `source_space`'s strips (in `virtual_index`
/// order) and appends them onto `survivor_space`'s selected strip.
fn migrate_strips_to_survivor(
    source_space: SpaceId,
    survivor_space: SpaceId,
    display_id: CGDirectDisplayID,
    world: &mut DesktopOpWorld,
) -> Option<Entity> {
    let raw_src = source_space.get();
    let raw_dst = survivor_space.get();
    let mut source_entities: Vec<(u32, Entity)> = world
        .strips
        .iter()
        .filter(|(_, s, _, _, _, _)| s.id() == raw_src && s.len() > 0)
        .map(|(entity, s, _, _, _, _)| (s.virtual_index, entity))
        .collect();
    if source_entities.is_empty() {
        return None;
    }
    source_entities.sort_unstable_by_key(|(v, _)| *v);

    let mut combined = LayoutStrip::new(raw_dst, 0);
    for (_, src_entity) in source_entities {
        if let Ok((_, mut src_strip, _, _, _, _)) = world.strips.get_mut(src_entity) {
            combined.append_strip(&mut src_strip);
        }
    }
    let migrated_first = combined.first();
    if combined.len() == 0 {
        return None;
    }

    let existing_dest = world
        .strips
        .iter()
        .filter(|(_, s, _, _, _, _)| s.id() == raw_dst && !s.is_fullscreen())
        .min_by_key(|(_, s, _, _, _, selected)| (!*selected, s.virtual_index))
        .map(|(entity, _, _, _, _, _)| entity);
    if let Some(dest_entity) = existing_dest {
        if let Ok((_, mut dest_strip, _, _, _, _)) = world.strips.get_mut(dest_entity) {
            dest_strip.append_strip(&mut combined);
        }
    } else if let Some((disp_entity, disp, _)) =
        world.displays.iter().find(|(_, d, _)| d.id() == display_id)
    {
        world
            .commands
            .spawn_layout_strip(combined, disp.bounds().min, disp_entity, false)
            .insert((SelectedVirtualMarker, ChildOf(disp_entity)));
    }
    migrated_first
}

/// Destroys an ordinary Desktop (`type == 0`, §5.5). When targeting the active
/// Desktop, switches to a neighbor Desktop first and destroys the vacated Space
/// once the switch confirms.
pub(super) fn handle_destroy(
    target: Option<&DesktopTarget>,
    empty_only: bool,
    bridge: &mut DesktopBridge,
    world: &mut DesktopOpWorld,
) {
    let Some(active_disp_id) = active_display_id(&world.displays) else {
        return;
    };
    let Ok(census) = bridge
        .read_census()
        .inspect_err(|err| warn!("desktop destroy census failed: {err}"))
    else {
        return;
    };
    let dest = match target {
        Some(t) => resolve_desktop_target(&census, active_disp_id, t, false),
        None => census
            .display(active_disp_id)
            .and_then(|d| census.find_space(d.current_space)),
    };
    let Some(dest) = dest else {
        debug!("desktop destroy target {target:?} not found");
        return;
    };
    if !dest.kind.is_desktop() {
        warn!(
            "desktop destroy refused: space {} is not a Desktop",
            dest.id
        );
        return;
    }
    let Some(disp) = census.display(dest.display_id) else {
        return;
    };
    if disp.desktop_count() < 2 {
        warn!(
            "desktop destroy refused: {} is the last Desktop on display {}",
            dest.id, disp.display_id
        );
        return;
    }
    let app_windows = match inspect_space_windows_for_destroy(dest.id, empty_only, bridge) {
        Ok(list) => list,
        Err(reason) => {
            warn!("desktop destroy refused: {reason}");
            return;
        }
    };

    if dest.active {
        destroy_active_desktop(
            dest,
            disp,
            active_disp_id,
            &census,
            app_windows,
            bridge,
            world,
        );
    } else {
        destroy_inactive_desktop(dest, disp, &census, app_windows, bridge, world);
    }
}

fn destroy_active_desktop(
    dest: &SpaceRecord,
    disp: &DisplaySpaces,
    active_disp_id: CGDirectDisplayID,
    census: &SpaceCensus,
    app_windows: Vec<WinID>,
    bridge: &mut DesktopBridge,
    world: &mut DesktopOpWorld,
) {
    let Some(neighbor) = adjacent_desktop(disp, dest.id) else {
        warn!(
            "desktop destroy refused: no neighbor Desktop found for {}",
            dest.id
        );
        return;
    };
    let hide = sibling_space_ids(census, neighbor);
    if let Err(err) = bridge.activate_space(&neighbor.skylight_display, neighbor.id, &hide) {
        warn!("desktop destroy switch to {} failed: {err}", neighbor.id);
        if err.request_may_have_applied {
            refresh_census_from_bridge(bridge, &mut world.census);
        }
        return;
    }
    let prior_neighbor_focus =
        preferred_focus_on_space(neighbor.id, &world.strips, world.focus_history.as_deref());
    world.pending.push(PendingDesktopOp::new(
        format!("vacate desktop {} -> {}", dest.id, neighbor.id),
        ConfirmCondition::SpaceActive(neighbor.id),
        Some(PostConfirmAction::DestroyVacatedSpace {
            space_id: dest.id,
            neighbor_id: neighbor.id,
            display_id: disp.display_id,
            activate_display: dest.display_id == active_disp_id,
            preferred_window: prior_neighbor_focus,
            migrated: app_windows,
        }),
    ));
}

fn destroy_inactive_desktop(
    dest: &SpaceRecord,
    disp: &DisplaySpaces,
    census: &SpaceCensus,
    app_windows: Vec<WinID>,
    bridge: &mut DesktopBridge,
    world: &mut DesktopOpWorld,
) {
    if let Err(err) = bridge.destroy_space(dest.id) {
        warn!("desktop destroy {} failed: {err}", dest.id);
        if err.request_may_have_applied {
            refresh_census_from_bridge(bridge, &mut world.census);
        }
        return;
    }
    let survivor = census
        .find_space(disp.current_space)
        .filter(|s| s.kind.is_desktop())
        .map(|s| s.id)
        .or_else(|| adjacent_desktop(disp, dest.id).map(|s| s.id));
    if let Some(survivor_id) = survivor {
        migrate_strips_to_survivor(dest.id, survivor_id, disp.display_id, world);
    }
    world.pending.push(PendingDesktopOp::new(
        format!("destroy desktop {}", dest.id),
        ConfirmCondition::SpaceAbsent {
            space_id: dest.id,
            migrated: app_windows,
        },
        Some(PostConfirmAction::DespawnDestroyedSpace(dest.id)),
    ));
}

fn validate_display_reorder<'a>(
    target: &DesktopTarget,
    census: &'a SpaceCensus,
    active_disp_id: CGDirectDisplayID,
) -> Result<(&'a DisplaySpaces, usize, usize), String> {
    let dest = resolve_desktop_target(census, active_disp_id, target, false)
        .ok_or_else(|| format!("target {target:?} out of range or not found"))?;
    let disp = census
        .display(dest.display_id)
        .ok_or_else(|| format!("display {} missing from census", dest.display_id))?;
    if !disp.all_desktops() {
        return Err(format!(
            "display {} contains fullscreen Spaces; desktop reorder/swap unsupported",
            disp.display_id
        ));
    }
    let source = census.find_space(disp.current_space).ok_or_else(|| {
        format!(
            "active space {} missing on display {}",
            disp.current_space, disp.display_id
        )
    })?;
    Ok((disp, source.space_index, dest.space_index))
}

/// Moves the display's active Desktop to `target`'s 0-based index (§5.10).
pub(super) fn handle_reorder(
    target: &DesktopTarget,
    bridge: &mut DesktopBridge,
    world: &mut DesktopOpWorld,
) {
    let Some(active_disp_id) = active_display_id(&world.displays) else {
        return;
    };
    let Ok(census) = bridge
        .read_census()
        .inspect_err(|err| warn!("desktop reorder census failed: {err}"))
    else {
        return;
    };
    let (disp, source_local, target_local) =
        match validate_display_reorder(target, &census, active_disp_id) {
            Ok(tuple) => tuple,
            Err(reason) => {
                warn!("desktop reorder refused: {reason}");
                return;
            }
        };
    if source_local == target_local {
        return;
    }

    let source_id = disp.spaces[source_local].id;
    let Ok(index_u32) = u32::try_from(target_local) else {
        return;
    };
    let mut expected: Vec<SpaceId> = disp.spaces.iter().map(|s| s.id).collect();
    let moved = expected.remove(source_local);
    expected.insert(target_local, moved);

    if let Err(err) = bridge.move_space_to_index(source_id, &disp.skylight_display, index_u32) {
        warn!("desktop reorder {source_id} -> index {index_u32} failed: {err}");
        if err.request_may_have_applied {
            refresh_census_from_bridge(bridge, &mut world.census);
        }
        return;
    }

    world.pending.push(PendingDesktopOp::new(
        format!("reorder desktop {source_id} to index {index_u32}"),
        ConfirmCondition::DisplayOrder {
            display_id: disp.display_id,
            expected,
            active_space: disp.current_space,
        },
        None,
    ));
}

/// Swaps the display's active Desktop with `target` using the §5.10 single-step
/// (adjacent) or two-step confirmed (non-adjacent) permutation sequence.
pub(super) fn handle_swap(
    target: &DesktopTarget,
    bridge: &mut DesktopBridge,
    world: &mut DesktopOpWorld,
) {
    let Some(active_disp_id) = active_display_id(&world.displays) else {
        return;
    };
    let Ok(census) = bridge
        .read_census()
        .inspect_err(|err| warn!("desktop swap census failed: {err}"))
    else {
        return;
    };
    let (disp, a_local, b_local) = match validate_display_reorder(target, &census, active_disp_id) {
        Ok(tuple) => tuple,
        Err(reason) => {
            warn!("desktop swap refused: {reason}");
            return;
        }
    };
    if a_local == b_local {
        return;
    }

    let low = a_local.min(b_local);
    let high = a_local.max(b_local);
    let (Ok(low_u32), Ok(high_u32)) = (u32::try_from(low), u32::try_from(high)) else {
        return;
    };
    let ids: Vec<SpaceId> = disp.spaces.iter().map(|s| s.id).collect();
    let earlier = ids[low];
    let later = ids[high];

    let mut step1_expected = ids.clone();
    let removed = step1_expected.remove(low);
    step1_expected.insert(high, removed);

    if let Err(err) = bridge.move_space_to_index(earlier, &disp.skylight_display, high_u32) {
        warn!("desktop swap step 1 ({earlier} -> {high_u32}) failed: {err}");
        if err.request_may_have_applied {
            refresh_census_from_bridge(bridge, &mut world.census);
        }
        return;
    }

    let on_confirmed = if high == low + 1 {
        None
    } else {
        let mut final_expected = ids;
        final_expected.swap(low, high);
        Some(PostConfirmAction::SecondSwapStep {
            space_id: later,
            skylight_display: disp.skylight_display.clone(),
            index: low_u32,
            display_id: disp.display_id,
            expected: final_expected,
            active_space: disp.current_space,
        })
    };

    world.pending.push(PendingDesktopOp::new(
        format!("swap desktop step 1 ({earlier} -> {high_u32})"),
        ConfirmCondition::DisplayOrder {
            display_id: disp.display_id,
            expected: step1_expected,
            active_space: disp.current_space,
        },
        on_confirmed,
    ));
}

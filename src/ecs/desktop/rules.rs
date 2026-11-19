//! Runtime `[windows.<rule>]` `desktop = N | "all"` spawn placement rules.

use std::collections::HashSet;

use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::query::{Added, Has, With, Without};
use bevy::ecs::system::{Commands, NonSendMut, Populated, Query, Res, ResMut, SystemParam};
use objc2_core_graphics::CGDirectDisplayID;
use tracing::debug;

use super::ops::active_display_id;
use super::pin::PinnedDesktopWindow;
use super::{
    ConfirmCondition, DesktopCensus, PendingDesktopOp, PendingDesktopOps, StickyWindowMarker,
    exec_bridge_op, join_window_to_desktops, refresh_census_from_bridge, select_desktop_strip,
    spawn_desktop_strip,
};
use crate::config::{Config, DesktopRuleTarget};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::{ActiveDisplayMarker, SelectedVirtualMarker, Unmanaged, WindowProperties};
use crate::manager::spaces::{DesktopBridge, DisplaySpaces, SpaceCensus, SpaceId};
use crate::manager::{Application, Display, Window};
use crate::platform::WinID;

/// One-frame spawn marker suppressing automatic `WindowFocused` synthesis when
/// a `[windows]` `desktop = N` rule sends a newly created window to an inactive Desktop.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct SentToInactiveDesktop;

type AddedRuleWindows<'w, 's> = Populated<
    'w,
    's,
    (Entity, &'static Window, &'static ChildOf),
    (Added<Window>, Without<StickyWindowMarker>),
>;
type RuleStrips<'w, 's> =
    Query<'w, 's, (Entity, &'static mut LayoutStrip, Has<SelectedVirtualMarker>), Without<Window>>;

#[derive(SystemParam)]
pub(super) struct SpawnRuleWorld<'w, 's> {
    apps: Query<'w, 's, &'static Application>,
    displays: Query<'w, 's, (Entity, &'static Display, Has<ActiveDisplayMarker>)>,
    strips: RuleStrips<'w, 's>,
    config: Res<'w, Config>,
    census: ResMut<'w, DesktopCensus>,
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
    if !world.config.has_desktop_window_rules() {
        return;
    }
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
        .any(|(_, strip, _)| strip.tabbed(entity))
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
    disp: &DisplaySpaces,
    memberships: Vec<SpaceId>,
    bridge: &mut DesktopBridge,
    world: &mut SpawnRuleWorld,
) {
    let (added, failed, may_have_applied) =
        join_window_to_desktops(wid, disp, &memberships, bridge, "window rule desktop='all'");
    if may_have_applied {
        refresh_census_from_bridge(bridge, &mut world.census);
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
    disp: &DisplaySpaces,
    memberships: &[SpaceId],
    bridge: &mut DesktopBridge,
    world: &mut SpawnRuleWorld,
) {
    let Some(dest) = disp.desktop_by_number(num.get()) else {
        debug!("window rule desktop={num}: out of range on display {disp_id}");
        return;
    };
    let moved = memberships != [dest.id];
    if moved {
        let res = bridge.move_windows_to_space(&[wid], dest.id);
        if exec_bridge_op(
            &format!("window rule desktop={num} move for {wid}"),
            res,
            bridge,
            &mut world.census,
        )
        .is_none()
        {
            return;
        }
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
    let target_strip = select_desktop_strip(world.strips.iter(), raw_dest);
    if let Some(strip_entity) = target_strip {
        if let Ok((_, mut strip, _)) = world.strips.get_mut(strip_entity) {
            strip.append(entity);
        }
    } else {
        let mut new_strip = LayoutStrip::new(raw_dest, 0);
        new_strip.append(entity);
        spawn_desktop_strip(
            new_strip,
            display_id,
            &world.displays,
            false,
            &mut world.commands,
        );
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

use std::collections::HashMap;
use std::time::Duration;

use bevy::app::{App, Plugin, PostUpdate};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::{Add, Remove};
use bevy::ecs::observer::On;
use bevy::ecs::query::{Added, Has, With};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{Commands, Populated, Query, Res, Single};
use bevy::prelude::Event as BevyEvent;
use bevy::time::common_conditions::on_timer;
use tracing::{Level, debug, error, instrument, trace, warn};

use super::{FocusedMarker, MouseHeldMarker, SystemTheme, Unmanaged};
use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{ActiveDisplay, GlobalState, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, Scrolling, SendMessageTrigger, SpawnCommandsExt, StrayFocusEvent,
};
use crate::events::Event;
use crate::manager::{Application, Display, Window, WindowManager};
use crate::platform::WorkspaceId;

const REFRESH_WINDOW_CHECK_FREQ_MS: u64 = 1000;

#[derive(Default)]
pub struct TierMemory {
    pub last_managed: Option<Entity>,
    pub last_floating: Option<Entity>,
}

/// Keyed by `WorkspaceId` so toggling on one Space can't reach a window last
/// focused on another. Cleared on entity despawn (`forget`) so recycled
/// Entity IDs can't resolve to the wrong window, and on workspace despawn
/// (`forget_workspace`) to bound the map.
#[derive(Default, Resource)]
pub struct FocusHistory {
    by_workspace: HashMap<WorkspaceId, TierMemory>,
}

impl FocusHistory {
    pub fn record(
        &mut self,
        workspace: WorkspaceId,
        entity: Entity,
        unmanaged: Option<&Unmanaged>,
    ) {
        let slot = self.by_workspace.entry(workspace).or_default();
        match unmanaged {
            None => slot.last_managed = Some(entity),
            Some(Unmanaged::Floating) => slot.last_floating = Some(entity),
            Some(_) => {}
        }
    }

    pub fn last_managed(&self, workspace: WorkspaceId) -> Option<Entity> {
        self.by_workspace
            .get(&workspace)
            .and_then(|t| t.last_managed)
    }

    pub fn last_floating(&self, workspace: WorkspaceId) -> Option<Entity> {
        self.by_workspace
            .get(&workspace)
            .and_then(|t| t.last_floating)
    }

    pub fn forget(&mut self, entity: Entity) {
        for slot in self.by_workspace.values_mut() {
            if slot.last_managed == Some(entity) {
                slot.last_managed = None;
            }
            if slot.last_floating == Some(entity) {
                slot.last_floating = None;
            }
        }
    }

    pub fn forget_workspace(&mut self, workspace: WorkspaceId) {
        self.by_workspace.remove(&workspace);
    }
}

pub struct FocusEventsPlugin;

impl Plugin for FocusEventsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FocusHistory>();
        app.add_systems(
            PostUpdate,
            (
                autocenter_window_on_focus.after(super::systems::animate_resize_entities),
                mouse_follows_focus.after(super::systems::animate_resize_entities),
                recover_lost_focus.run_if(on_timer(Duration::from_millis(
                    REFRESH_WINDOW_CHECK_FREQ_MS,
                ))),
            ),
        );
        app.add_observer(dim_remove_window_trigger)
            .add_observer(dim_window_trigger)
            .add_observer(maintain_focus_singleton)
            .add_observer(virtual_strip_activated)
            .add_observer(stray_focus_observer)
            .add_observer(focus_window_trigger);
    }
}

#[derive(BevyEvent)]
pub(super) struct FocusWindow {
    pub entity: Entity,
    pub raise: bool,
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn maintain_focus_singleton(
    trigger: On<Add, FocusedMarker>,
    windows: Query<(Entity, Has<FocusedMarker>), With<Window>>,
    mut config: GlobalState,
    mut commands: Commands,
) {
    let focused_entity = trigger.event().entity;

    for (entity, focused) in windows {
        if focused
            && entity != focused_entity
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            debug!("window {entity} lost focus.");
            entity_commands.try_remove::<FocusedMarker>();
        }
    }

    // Check if the reshuffle was caused by a keyboard switch or mouse move.
    // Skip reshuffle if caused by mouse - because then it won't center.
    if config.ffm_flag().is_none() {
        config.set_skip_reshuffle(false);
    }
    config.set_ffm_flag(None);
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn autocenter_window_on_focus(
    focused: Single<Entity, Added<FocusedMarker>>,
    mouse_held: Query<&MouseHeldMarker>,
    windows: Windows,
    global_state: GlobalState,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
) {
    let entity = *focused;

    if global_state.skip_reshuffle() || global_state.initializing() || !mouse_held.is_empty() {
        return;
    }
    if active_display.active_strip().tabbed(entity) {
        return;
    }
    if config.auto_center()
        && let Some((_, _, None)) = windows.get_managed(entity)
        && let Some(size) = windows.size(entity)
        && let Some(mut origin) = windows.origin(entity)
    {
        let center = active_display.bounds().center();
        origin.x = center.x - size.x / 2;
        commands.reposition_entity(entity, origin);
    }
    commands.reshuffle_around(entity);
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn mouse_follows_focus(
    focused: Single<Entity, Added<FocusedMarker>>,
    windows: Windows,
    global_state: GlobalState,
    config: Res<Config>,
    window_manager: Res<WindowManager>,
    displays: Query<&Display>,
    workspaces: Query<(
        &LayoutStrip,
        &ChildOf,
        Option<&Scrolling>,
        Has<ActiveWorkspaceMarker>,
    )>,
) {
    let entity = *focused;
    let Some(window) = windows.get(entity) else {
        return;
    };
    if workspaces
        .iter()
        .find_map(|(_, _, scrolling, active)| if active { scrolling } else { None })
        .is_some_and(|scrolling| scrolling.is_user_swiping)
    {
        debug!("Suppressing center mouse due to a swipe");
        return;
    }

    trace!(
        "window {}, skip_reshuffle {}, ffm flag {:?}.",
        window.id(),
        global_state.skip_reshuffle(),
        global_state.ffm_flag()
    );
    if config.mouse_follows_focus()
        && !global_state.skip_reshuffle()
        && global_state.ffm_flag().is_none_or(|id| id != window.id())
        && let Some(frame) = windows.moving_frame(entity)
        && let Some(display_bounds) = workspaces
            .into_iter()
            .find_map(|(strip, child, _, _)| strip.contains(entity).then_some(child))
            .and_then(|child| displays.get(child.parent()).ok())
            .map(Display::bounds)
    {
        let visible = display_bounds.intersect(frame);
        // If the overlap is smaller than 50x50, the window is probably hidden
        // off screen, so do not move the mouse.
        if visible.size().length_squared() > 5000 {
            let origin = visible.center();
            debug!("centering on {} {origin}", window.id());
            window_manager.warp_mouse(origin);
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn dim_window_trigger(
    trigger: On<Add, FocusedMarker>,
    windows: Windows,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    theme: Option<Res<SystemTheme>>,
) {
    let Some(window) = windows.get(trigger.event().entity) else {
        return;
    };

    let dark = theme.is_some_and(|theme| theme.is_dark);
    if config.window_dim_ratio(dark).is_some() {
        window_manager.dim_windows(&[window.id()], 0.0);
    }
}

#[allow(clippy::needless_pass_by_value)]
fn dim_remove_window_trigger(
    trigger: On<Remove, FocusedMarker>,
    windows: Windows,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    theme: Option<Res<SystemTheme>>,
) {
    let Some((window, _, None)) = windows.get_managed(trigger.event().entity) else {
        return;
    };

    let same_display = active_display
        .active_strip()
        .contains(trigger.event().entity);
    if !same_display {
        // Do not dim the window loosing focus on another display.
        return;
    }

    let dark = theme.is_some_and(|theme| theme.is_dark);
    if let Some(dim_ratio) = config.window_dim_ratio(dark) {
        window_manager.dim_windows(&[window.id()], dim_ratio);
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn virtual_strip_activated(
    trigger: On<Add, FocusedMarker>,
    workspaces: Query<(Entity, &LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    mut commands: Commands,
) {
    let owner_strip = workspaces.into_iter().find_map(|(entity, strip, active)| {
        (strip.contains(trigger.entity) && !active).then_some(entity)
    });
    if let Some(entity) = owner_strip
        && let Ok(mut entity_commands) = commands.get_entity(entity)
    {
        entity_commands.try_insert(ActiveWorkspaceMarker);
    }
}

#[allow(clippy::needless_pass_by_value)]
fn focus_window_trigger(trigger: On<FocusWindow>, windows: Windows, apps: Query<&Application>) {
    let FocusWindow { entity, raise } = *trigger.event();
    let Some(window) = windows.get(entity) else {
        return;
    };
    let Some(psn) = windows.psn(window.id(), &apps) else {
        return;
    };
    if !raise
        && let Some((focused_window, _)) = windows.focused()
        && let Some(focused_psn) = windows.psn(focused_window.id(), &apps)
    {
        window.focus_without_raise(psn, focused_window, focused_psn);
    } else {
        window.focus_with_raise(psn);
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
fn recover_lost_focus(
    windows: Windows,
    active_workspace: Query<&LayoutStrip, With<ActiveWorkspaceMarker>>,
    mut commands: Commands,
) {
    if windows.focused().is_some() {
        return;
    }
    error!("Lost focus marker, recovering!");
    if let Ok(strip) = active_workspace
        .single()
        .inspect_err(|err| error!("Unable to get current workspace: {err}"))
        && let Some(entity) = strip.first().ok().and_then(|col| col.top())
    {
        commands.focus_entity(entity, false);
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn stray_focus_observer(
    trigger: On<Add, Window>,
    focus_events: Populated<(Entity, &StrayFocusEvent)>,
    windows: Windows,
    mut commands: Commands,
) {
    let entity = trigger.event().entity;
    let Some(window_id) = windows.get(entity).map(|window| window.id()) else {
        return;
    };

    focus_events
        .iter()
        .filter(|(_, stray_focus)| stray_focus.0 == window_id)
        .for_each(|(timeout_entity, _)| {
            debug!("Re-queueing lost focus event for window id {window_id}.");
            commands.trigger(SendMessageTrigger(Event::WindowFocused { window_id }));
            commands.entity(timeout_entity).despawn();
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::world::World;

    #[test]
    fn record_and_read_per_tier() {
        let mut world = World::new();
        let managed = world.spawn(()).id();
        let floating = world.spawn(()).id();
        let mut history = FocusHistory::default();

        history.record(1, managed, None);
        history.record(1, floating, Some(&Unmanaged::Floating));

        assert_eq!(history.last_managed(1), Some(managed));
        assert_eq!(history.last_floating(1), Some(floating));
    }

    #[test]
    fn record_ignores_minimized_and_hidden() {
        let mut world = World::new();
        let entity = world.spawn(()).id();
        let mut history = FocusHistory::default();

        history.record(1, entity, Some(&Unmanaged::Minimized));
        history.record(1, entity, Some(&Unmanaged::Hidden));

        assert_eq!(history.last_managed(1), None);
        assert_eq!(history.last_floating(1), None);
    }

    #[test]
    fn per_workspace_isolation() {
        let mut world = World::new();
        let a = world.spawn(()).id();
        let b = world.spawn(()).id();
        let mut history = FocusHistory::default();

        history.record(1, a, None);
        history.record(2, b, None);

        assert_eq!(history.last_managed(1), Some(a));
        assert_eq!(history.last_managed(2), Some(b));
    }

    #[test]
    fn forget_clears_entity_across_workspaces() {
        let mut world = World::new();
        let target = world.spawn(()).id();
        let other = world.spawn(()).id();
        let mut history = FocusHistory::default();

        history.record(1, target, None);
        history.record(2, target, Some(&Unmanaged::Floating));
        history.record(2, other, None);

        history.forget(target);

        assert_eq!(history.last_managed(1), None);
        assert_eq!(history.last_floating(2), None);
        assert_eq!(history.last_managed(2), Some(other));
    }

    #[test]
    fn forget_workspace_drops_entry() {
        let mut world = World::new();
        let entity = world.spawn(()).id();
        let mut history = FocusHistory::default();

        history.record(1, entity, None);
        history.forget_workspace(1);

        assert_eq!(history.last_managed(1), None);
    }
}

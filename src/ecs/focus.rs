use std::collections::HashMap;
use std::time::Duration;

use bevy::app::{App, Plugin, PostUpdate, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::{Add, Remove};
use bevy::ecs::observer::On;
use bevy::ecs::query::{Added, Has, With};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{Commands, Populated, Query, Res, Single};
use bevy::math::IRect;
use bevy::prelude::Event as BevyEvent;
use bevy::time::common_conditions::on_timer;
use tracing::{Level, debug, error, info, instrument, trace, warn};

use super::{FocusedMarker, MouseHeldMarker, SystemTheme, Timeout, Unmanaged, VerifyFocus};
use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{ActiveDisplay, GlobalState, WindowCtx, Windows};
use crate::ecs::workspace::RestoreFocusMarker;
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, Position, RaiseWindow, ResizeMarker, Scrolling,
    SendMessageTrigger, SpawnCommandsExt, StrayFocusEvent,
};
use crate::events::Event;
use crate::manager::{Application, Display, Window, WindowManager};
use crate::platform::WorkspaceId;

const REFRESH_WINDOW_CHECK_FREQ_MS: u64 = 1000;
pub(crate) const VERIFY_FOCUS_TIMEOUT: Duration = Duration::from_millis(300);

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
            Update,
            (
                detect_focus_rejection.before(super::systems::timeout_ticker),
                fix_window_size_on_focus,
            ),
        );
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
            .add_observer(maintain_verify_focus_singleton)
            .add_observer(virtual_strip_activated)
            .add_observer(stray_focus_observer)
            .add_observer(focus_window_trigger)
            .add_observer(raise_window_trigger);
    }
}

#[derive(BevyEvent)]
pub(super) struct FocusWindow {
    pub entity: Entity,
    pub raise: bool,
}

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

/// Whether two windows are members of one native tab group: the app shows one
/// of them at a time, and focusing any of them can leave the focus on the one
/// the app decided to show.
///
/// The strip knows the ones it has already grouped. The rest are recognised the
/// same way [`super::systems::detect_tabbed_windows`] recognises them in the
/// first place: same app, same frame.
fn shares_a_tab_group(
    workspaces: &Query<(Entity, &mut LayoutStrip)>,
    windows: &Windows,
    target: Entity,
    actual: Entity,
) -> bool {
    if workspaces.iter().any(|(_, strip)| {
        strip
            .tab_group(target)
            .is_some_and(|group| group.contains(&actual))
    }) {
        return true;
    }

    let parent_of = |entity: Entity| {
        windows
            .get(entity)
            .and_then(|window| windows.find_parent(window.id()))
            .map(|(_, _, parent)| parent)
    };
    let (Some(target_app), Some(actual_app)) = (parent_of(target), parent_of(actual)) else {
        return false;
    };
    if target_app != actual_app {
        return false;
    }

    windows
        .frame(target)
        .zip(windows.frame(actual))
        .is_some_and(|(target_frame, actual_frame)| {
            target_frame.min.chebyshev_distance(actual_frame.min) <= 1
                && target_frame.size().chebyshev_distance(actual_frame.size()) <= 1
        })
}

#[instrument(level = Level::DEBUG, skip_all, fields(focused))]
fn fix_window_size_on_focus(
    focused: Single<Entity, Added<FocusedMarker>>,
    mut windows: Query<(&mut Window, &mut Bounds, Has<ResizeMarker>)>,
) {
    if let Ok((mut window, mut bounds, resizing)) = windows.get_mut(*focused)
        && !resizing
        && let Ok(frame) = window.update_frame()
        && frame.size() != bounds.0
    {
        debug!("fixing window {} size!", window.id());
        bounds.0 = frame.size();
    }
}

/// Ensures at most one window entity carries [`VerifyFocus`] at any time, so
/// rapid focus switching cancels verification on intermediate windows.
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn maintain_verify_focus_singleton(
    trigger: On<Add, VerifyFocus>,
    verifying: Query<Entity, With<VerifyFocus>>,
    mut commands: Commands,
) {
    let target = trigger.event().entity;
    for entity in verifying {
        if entity != target
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            entity_commands.try_remove::<(Timeout, VerifyFocus)>();
        }
    }
}

#[instrument(level = Level::DEBUG, skip_all)]
fn detect_focus_rejection(
    pending: Populated<(Entity, &Window, &ChildOf, &Timeout), With<VerifyFocus>>,
    apps: Query<&Application>,
    mut workspaces: Query<(Entity, &mut LayoutStrip)>,
    windows: Windows,
    mut commands: Commands,
) {
    for (target_entity, target_window, child_of, timeout) in &pending {
        if !timeout.timer.is_finished() {
            continue;
        }
        let Ok(app) = apps.get(child_of.parent()) else {
            continue;
        };
        // Only treat as a rejection if the target window's application is
        // currently frontmost and reports a different window of the same app
        // as focused.
        if !app.is_frontmost() {
            continue;
        }
        let Ok(actual_window_id) = app.focused_window_id() else {
            continue;
        };
        if actual_window_id == target_window.id() {
            continue;
        }
        let Some((actual_window, actual_entity, actual_parent)) =
            windows.find_parent(actual_window_id)
        else {
            continue;
        };
        if actual_parent != child_of.parent() {
            continue;
        }
        // If focus in the ECS world has already moved to a third window (neither
        // `target_entity` nor `actual_entity`), do not treat it as a rejection.
        if windows
            .focused()
            .is_some_and(|(_, focused)| focused != target_entity && focused != actual_entity)
        {
            continue;
        }

        // Native tabs share one slot: asking for a background tab makes the app
        // select it, and the focus notification names whichever tab of the group
        // the app ended up showing. That is the app doing what was asked, not
        // refusing it — floating the window here is how a tabbed terminal ends up
        // scattered across the layout as windows nothing tiles.
        if shares_a_tab_group(&workspaces, &windows, target_entity, actual_entity) {
            debug!(
                "focus landed on tab sibling {actual_entity} of {target_entity}; not a rejection.",
            );
            continue;
        }

        info!(
            "focus rejection detected: requested window {} ({target_entity}), app focused window {} ({actual_entity}) instead; floating {target_entity}.",
            target_window.id(),
            actual_window.id(),
        );
        if let Ok(mut entity_commands) = commands.get_entity(target_entity) {
            entity_commands
                .try_remove::<(Timeout, VerifyFocus)>()
                .try_insert(Unmanaged::Floating);
        }
        for (_, mut strip) in &mut workspaces {
            if strip.contains(target_entity) {
                strip.remove(target_entity);
            }
        }
        commands.focus_entity(actual_entity, false);
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn autocenter_window_on_focus(
    focused: Single<Entity, Added<FocusedMarker>>,
    mouse_held: Query<&MouseHeldMarker>,
    restored: Query<&RestoreFocusMarker>,
    global_state: GlobalState,
    active_display: ActiveDisplay,
    mut ctx: WindowCtx,
) {
    let entity = *focused;

    // Skip auto-centering when this focus came from a workspace restore, since
    // the strip is already at its saved origin. window_focused_trigger and
    // timeout_ticker are responsible for clearing the marker.
    if restored.iter().any(|marker| marker.entity == entity) {
        return;
    }

    if global_state.skip_reshuffle() || global_state.initializing() || !mouse_held.is_empty() {
        return;
    }
    if active_display.active_strip().tabbed(entity) {
        return;
    }
    if ctx.config.auto_center()
        && let Some((_, _, None)) = ctx.windows.get_managed(entity)
        && let Some(size) = ctx.windows.size(entity)
        && let Some(mut origin) = ctx.windows.origin(entity)
    {
        let center = active_display.bounds().center();
        origin.x = center.x - size.x / 2;
        ctx.commands.reposition_entity(entity, origin);
    }
    ctx.commands.reshuffle_around(entity);
}

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

fn focus_window_trigger(
    trigger: On<FocusWindow>,
    windows: Windows,
    apps: Query<&Application>,
    verifying: Query<Entity, With<VerifyFocus>>,
    mut commands: Commands,
) {
    let FocusWindow { entity, raise } = *trigger.event();
    for other in verifying {
        if other != entity
            && let Ok(mut entity_commands) = commands.get_entity(other)
        {
            entity_commands.try_remove::<(Timeout, VerifyFocus)>();
        }
    }
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

fn raise_window_trigger(
    trigger: On<RaiseWindow>,
    windows: Query<(Entity, &Window, &Position, &Bounds)>,
    active_display: ActiveDisplay,
    config: Res<Config>,
) {
    let RaiseWindow { entity, with_strip } = *trigger.event();

    let Ok((focus, window, _, _)) = windows.get(entity) else {
        return;
    };

    if with_strip {
        let viewport = active_display.actual_bounds(&config);
        let strip = active_display.active_strip();
        strip
            .all_windows()
            .into_iter()
            .filter_map(|entity| {
                if entity == focus {
                    None
                } else {
                    windows.get(entity).ok()
                }
            })
            .filter(|(_, _, origin, size)| {
                let frame = IRect::from_corners(origin.0, origin.0 + size.0);
                viewport.intersect(frame).width() > 50
            })
            .for_each(|(_, window, _, _)| {
                window.raise_without_focus();
            });
    }

    // Raise the focused window last, because raised windows get OS focus events.
    window.raise_without_focus();
}

#[instrument(level = Level::DEBUG, skip_all)]
fn recover_lost_focus(
    windows: Windows,
    active_workspace: Query<&LayoutStrip, With<ActiveWorkspaceMarker>>,
    mut commands: Commands,
) {
    if windows.focused().is_some() {
        return;
    }
    if let Ok(strip) = active_workspace
        .single()
        .inspect_err(|err| error!("Unable to get current workspace: {err}"))
        && let Some(entity) = strip.first().ok().and_then(|col| col.top())
    {
        error!("Lost focus marker, recovering!");
        commands.focus_entity(entity, false);
    }
}

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
            if let Ok(mut entity_commands) = commands.get_entity(timeout_entity) {
                entity_commands.try_despawn();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::MockWindowApi;
    use crate::manager::app::MockApplicationApi;
    use crate::platform::WinID;
    use bevy::ecs::world::World;

    fn spawn_app(world: &mut World, frontmost: bool, focused_id: WinID) -> Entity {
        let mut api = MockApplicationApi::new();
        api.expect_is_frontmost().returning(move || frontmost);
        api.expect_focused_window_id()
            .returning(move || Ok(focused_id));
        world.spawn(Application::new(Box::new(api))).id()
    }

    fn spawn_window(world: &mut World, app: Entity, id: WinID) -> Entity {
        let mut api = MockWindowApi::new();
        api.expect_id().returning(move || id);
        world
            .spawn((
                Window::new(Box::new(api)),
                Position(bevy::math::IVec2::ZERO),
                Bounds(bevy::math::IVec2::new(800, 600)),
                ChildOf(app),
            ))
            .id()
    }

    fn expired_verify_timeout() -> Timeout {
        let mut timeout = Timeout::for_component::<VerifyFocus>(VERIFY_FOCUS_TIMEOUT);
        timeout.timer.tick(VERIFY_FOCUS_TIMEOUT);
        timeout
    }

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

    /// Native tabs share a slot: the app answering with a sibling of the tab
    /// group is it doing what was asked, so the requested window must keep its
    /// place in the layout.
    #[test]
    fn focus_landing_on_a_tab_sibling_is_not_a_rejection() {
        let mut world = World::new();
        let app = spawn_app(&mut world, true, 2);
        let target = spawn_window(&mut world, app, 1);
        let sibling = spawn_window(&mut world, app, 2);

        let mut strip = LayoutStrip::default();
        strip.append(target);
        strip
            .convert_to_tabs(target, sibling)
            .expect("target is in the strip");
        world.spawn(strip);

        world
            .entity_mut(target)
            .insert((FocusedMarker, VerifyFocus, expired_verify_timeout()));

        let system_id = world.register_system(detect_focus_rejection);
        _ = world.run_system(system_id);

        assert!(
            world.get::<Unmanaged>(target).is_none(),
            "a tab sibling taking the focus must not float the requested tab"
        );
        let mut strips = world.query::<&LayoutStrip>();
        assert!(
            strips.single(&world).expect("one strip").contains(target),
            "and must not take it out of the layout"
        );
    }

    #[test]
    fn focus_rejection_floats_target_and_clears_verify_focus() {
        let mut world = World::new();
        let app = spawn_app(&mut world, true, 2);
        let target = spawn_window(&mut world, app, 1);
        let actual = spawn_window(&mut world, app, 2);
        // Move actual so it does not share a frame with target (not a tab group).
        world
            .entity_mut(actual)
            .insert(Position(bevy::math::IVec2::new(800, 0)));

        let mut strip = LayoutStrip::default();
        strip.append(target);
        strip.append(actual);
        world.spawn(strip);

        world
            .entity_mut(target)
            .insert((FocusedMarker, VerifyFocus, expired_verify_timeout()));

        let system_id = world.register_system(detect_focus_rejection);
        _ = world.run_system(system_id);

        assert!(
            world
                .get::<Unmanaged>(target)
                .is_some_and(|u| matches!(u, Unmanaged::Floating))
        );
        assert!(world.get::<VerifyFocus>(target).is_none());
        assert!(world.get::<Timeout>(target).is_none());
    }

    #[test]
    fn rapid_switching_cancels_verify_focus_on_previous_window() {
        let mut world = World::new();
        world.add_observer(maintain_verify_focus_singleton);

        let app = spawn_app(&mut world, true, 2);
        let first = spawn_window(&mut world, app, 1);
        let second = spawn_window(&mut world, app, 2);

        world
            .entity_mut(first)
            .insert((VerifyFocus, expired_verify_timeout()));
        world
            .entity_mut(second)
            .insert((VerifyFocus, expired_verify_timeout()));
        world.flush();

        assert!(
            world.get::<VerifyFocus>(first).is_none(),
            "arming VerifyFocus on a new window must cancel it on the previous window"
        );
        assert!(world.get::<Timeout>(first).is_none());
        assert!(world.get::<VerifyFocus>(second).is_some());
    }
}

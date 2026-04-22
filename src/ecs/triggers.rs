use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::{ChildOf, Children};
use bevy::ecs::lifecycle::{Add, Remove};
use bevy::ecs::message::MessageWriter;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Has, With};
use bevy::ecs::system::{Commands, NonSend, NonSendMut, Populated, Query, Res, ResMut, Single};
use bevy::math::IRect;
use notify::event::{DataChange, MetadataKind, ModifyKind};
use notify::{EventKind, Watcher};
use objc2_app_kit::NSScreen;
use objc2_foundation::{NSNumber, NSString, ns_string};
use std::pin::Pin;
use std::time::Duration;
use tracing::{Level, debug, error, info, instrument, trace, warn};

use super::{
    ActiveDisplayMarker, BProcess, FocusedMarker, FreshMarker, MissionControlActive,
    RetryFrontSwitch, SpawnWindowTrigger, StrayFocusEvent, SystemTheme, Timeout, Unmanaged,
    WMEventTrigger,
};
use crate::config::{Config, WindowParams};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{ActiveDisplay, ActiveDisplayMut, Configuration, Windows};
use crate::ecs::state::PaneruState;
use crate::ecs::workspace::PreviousStripPosition;
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, DockPosition, LayoutPosition, LocateDockTrigger, Position,
    RestoreWindowState, Scrolling, SendMessageTrigger, WidthRatio, WindowProperties, focus_entity,
    reposition_entity, reshuffle_around, resize_entity,
};
use crate::events::Event;
use crate::manager::{
    Application, Display, Origin, Process, Size, Window, WindowManager, WindowPadding, irect_from,
};
use crate::platform::{PlatformCallbacks, WinID};
use crate::util::symlink_target;

/// Computes the passthrough keybinding set for the given window/app and
/// publishes it to the input thread. Called on focus change and config reload.
fn update_passthrough(window: &Window, app: &Application, config: &Config) {
    let properties = WindowProperties::new(app, window, config);
    crate::platform::input::set_focused_passthrough(properties.passthrough_keys());
}

/// Handles display change events.
///
/// When the active display or space changes, this function ensures that the window manager's
/// internal state is updated. It marks the new active display with `FocusedMarker` and moves
/// the focused window to the correct `LayoutStrip` if it has been moved to a different display
/// or workspace.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the display change event.
/// * `focused_window` - A query for the currently focused window.
/// * `displays` - A query for all displays, with their focus state.
/// * `main_cid` - The main connection ID resource.
/// * `commands` - Bevy commands to manage components and trigger events.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn display_change_trigger(
    trigger: On<WMEventTrigger>,
    displays: Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let Event::DisplayChanged = trigger.event().0 else {
        return;
    };

    let Ok(active_id) = window_manager.active_display_id() else {
        error!("Unable to get active display id!");
        return;
    };

    for (display, entity, focused) in displays {
        let display_id = display.id();
        if focused && display_id != active_id {
            debug!("Display id {display_id} no longer active");
            if let Ok(mut cmd) = commands.get_entity(entity) {
                cmd.try_remove::<ActiveDisplayMarker>();
            }
        }
        if !focused && display_id == active_id {
            debug!("Display id {display_id} is active");
            if let Ok(mut cmd) = commands.get_entity(entity) {
                cmd.try_insert(ActiveDisplayMarker);
            }
        }
    }
    commands.trigger(WMEventTrigger(Event::SpaceChanged));
}

/// Handles the event when an application switches to the front. It updates the focused window and PSN.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the application front switched event.
/// * `processes` - A query for all processes with their children.
/// * `applications` - A query for all applications.
/// * `focused_window` - A query for the focused window.
/// * `focus_follows_mouse_id` - The resource to track focus follows mouse window ID.
/// * `commands` - Bevy commands to trigger events and manage components.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn front_switched_trigger(
    trigger: On<WMEventTrigger>,
    processes: Query<(&BProcess, &Children)>,
    applications: Query<&Application>,
    window_manager: Res<WindowManager>,
    mut config: Configuration,
    mut commands: Commands,
) {
    const FRONT_SWITCH_RETRY_SEC: u64 = 2;
    let Event::ApplicationFrontSwitched { ref psn } = trigger.event().0 else {
        return;
    };
    let Some((BProcess(process), children)) =
        processes.iter().find(|process| &process.0.psn() == psn)
    else {
        error!("Unable to find process with PSN {psn:?}");
        return;
    };

    if children.len() > 1 {
        warn!("Multiple apps registered to process '{}'.", process.name());
    }
    let Some(&app_entity) = children.first() else {
        error!("No application for process '{}'.", process.name());
        return;
    };
    let Some(app) = applications.get(app_entity).ok() else {
        error!("No application for process '{}'.", process.name());
        return;
    };
    debug!("front switching process: {}", process.name());

    if let Ok(focused_id) = app.focused_window_id().inspect_err(|err| {
        warn!("can not get current focus: {err}");
    }) {
        if let Some(point) = window_manager.cursor_position()
            && window_manager
                .find_window_at_point(&point)
                .is_ok_and(|window_id| window_id != focused_id)
        {
            // Window got focus without mouse movement - probably with a Cmd-Tab.
            // If so, bring it into view.
            config.set_skip_reshuffle(false);
            config.set_ffm_flag(None);
        }
        commands.trigger(WMEventTrigger(Event::WindowFocused {
            window_id: focused_id,
        }));
    } else {
        // Transient AX error (e.g. kAXErrorCannotComplete during app transitions).
        // Schedule a retry to query the focused window once the app is ready.
        let timeout = Timeout::new(
            Duration::from_secs(FRONT_SWITCH_RETRY_SEC),
            Some(format!(
                "Front switch retry for '{}' timed out.",
                process.name()
            )),
        );
        commands.spawn((timeout, RetryFrontSwitch(app_entity)));
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn theme_change_trigger(
    trigger: On<WMEventTrigger>,
    windows: Windows,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut theme: Option<ResMut<SystemTheme>>,
) {
    let Event::ThemeChanged = trigger.event().0 else {
        return;
    };
    let Some(ref mut theme) = theme else {
        return;
    };

    let is_dark = crate::util::is_dark_mode();
    if theme.is_dark == is_dark {
        return;
    }
    theme.is_dark = is_dark;
    info!("System theme changed: dark_mode={is_dark}");

    let Some(dim_ratio) = config.window_dim_ratio(is_dark) else {
        return;
    };

    // Re-apply dimming to all windows that are NOT focused.
    let focused_id = windows.focused().map(|(window, _)| window.id());
    let windows_to_dim: Vec<WinID> = windows
        .iter()
        .filter(|(window, _)| Some(window.id()) != focused_id)
        .map(|(window, _)| window.id())
        .collect();

    if !windows_to_dim.is_empty() {
        window_manager.dim_windows(&windows_to_dim, dim_ratio);
    }
}

/// Handles the event when a window gains focus. It updates the focused window, PSN, and reshuffles windows.
/// It also centers the mouse on the focused window if focus-follows-mouse is enabled.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the window focused event.
/// * `applications` - A query for all applications.
/// * `windows` - A query for all windows with their parent and focus state.
/// * `main_cid` - The main connection ID resource.
/// * `focus_follows_mouse_id` - The resource to track focus follows mouse window ID.
/// * `skip_reshuffle` - The resource to indicate if reshuffling should be skipped.
/// * `commands` - Bevy commands to manage components and trigger events.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn window_focused_trigger(
    trigger: On<WMEventTrigger>,
    applications: Query<&Application>,
    windows: Windows,
    mut active_display: ActiveDisplayMut,
    config: Configuration,
    mut commands: Commands,
) {
    const STRAY_FOCUS_RETRY_SEC: u64 = 2;

    let Event::WindowFocused { window_id } = trigger.event().0 else {
        return;
    };

    let Some((window, entity, parent)) = windows.find_parent(window_id) else {
        let timeout = Timeout::new(Duration::from_secs(STRAY_FOCUS_RETRY_SEC), None);
        commands.spawn((timeout, StrayFocusEvent(window_id)));
        return;
    };

    let Ok(app) = applications.get(parent) else {
        warn!("Unable to get parent for window {}.", window.id());
        return;
    };

    // Always keep passthrough in sync. An internal focus_entity call races
    // with the OS WindowFocused event; without this the passthrough keys
    // remain stale from a previously focused window.
    update_passthrough(window, app, config.config());

    if let Some((focused, _)) = windows.focused()
        && focused.id() == window_id
    {
        return;
    }

    // Guard against stale focus events. Without these checks, delayed
    // events (e.g. from RetryFrontSwitch or dont_focus re-assertions)
    // can pull FocusedMarker back to an old window after focus has moved on.
    //
    // 1. Cross-app: skip if the window's app is no longer frontmost.
    // 2. Same-app: skip if the app's current focused window differs from
    //    this event's window_id (the event is outdated).
    if !app.is_frontmost() {
        return;
    }
    if app.focused_window_id().is_ok_and(|id| id != window_id) {
        return;
    }

    // Handle tab switching: if the focused window is a tab, make it the leader.
    let layout_strip = active_display.active_strip();
    if let Ok(index) = layout_strip.index_of(entity)
        && let Some(column) = layout_strip.get_column_mut(index)
    {
        column.move_to_front(entity);
    }

    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        entity_commands.try_insert(FocusedMarker);
        debug!("window {} ({entity}) focused.", window.id());
    }
}

/// Handles Mission Control events, updating the `MissionControlActive` resource.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the Mission Control event.
/// * `mission_control_active` - The `MissionControlActive` resource.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn mission_control_trigger(
    trigger: On<WMEventTrigger>,
    windows: Windows,
    mut workspaces: Query<(
        Entity,
        &mut LayoutStrip,
        Has<ActiveWorkspaceMarker>,
        Option<&Scrolling>,
    )>,
    mut mission_control_active: ResMut<MissionControlActive>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    match trigger.event().0 {
        Event::MissionControlShowAllWindows
        | Event::MissionControlShowFrontWindows
        | Event::MissionControlShowDesktop => {
            mission_control_active.as_mut().0 = true;
            for (entity, _, _, scroll) in workspaces {
                if scroll.is_some() {
                    commands.entity(entity).try_remove::<Scrolling>();
                }
            }
        }
        Event::MissionControlExit => {
            mission_control_active.as_mut().0 = false;

            // Check if some windows disappeared from the current workspace
            // - e.g. they were moved away during mission control.
            if let Some(mut active_strip) = workspaces
                .iter_mut()
                .find_map(|(_, strip, active, _)| active.then_some(strip))
                && let Ok(present_windows) = window_manager.windows_in_workspace(active_strip.id())
            {
                let moved_windows = active_strip
                    .all_windows()
                    .into_iter()
                    .filter_map(|entity| windows.get(entity).zip(Some(entity)))
                    .filter(|(window, _)| !present_windows.contains(&window.id()));
                for (window, entity) in moved_windows {
                    debug!(
                        "window {} {entity} moved, removing from workspace {}",
                        window.id(),
                        active_strip.id(),
                    );
                    // Simply removing them from the current strip is enough,
                    // they will be re-detected during the workspace change.
                    active_strip.remove(entity);
                }
            }
        }
        _ => (),
    }
}

/// Dispatches process-related messages, such as application launch and termination.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the application event.
/// * `processes` - A query for all processes.
/// * `commands` - Bevy commands to spawn or despawn entities.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn application_event_trigger(
    trigger: On<WMEventTrigger>,
    processes: Query<(&BProcess, Entity)>,
    mut commands: Commands,
) {
    const PROCESS_READY_TIMEOUT_SEC: u64 = 5;
    let find_process = |psn| {
        processes
            .iter()
            .find(|(BProcess(process), _)| &process.psn() == psn)
    };

    match &trigger.event().0 {
        Event::ApplicationLaunched { psn, observer } if find_process(psn).is_none() => {
            let process: BProcess = Process::new(psn, observer.clone()).into();
            let timeout = Timeout::new(
                Duration::from_secs(PROCESS_READY_TIMEOUT_SEC),
                Some(format!(
                    "Process '{}' did not become ready in {PROCESS_READY_TIMEOUT_SEC}s.",
                    process.name()
                )),
            );
            commands.spawn((FreshMarker, timeout, process));
        }

        Event::ApplicationTerminated { psn } => {
            if let Some((_, entity)) = find_process(psn) {
                commands.entity(entity).despawn();
            }
        }
        _ => (),
    }
}

/// Dispatches application-related messages, such as window creation, destruction, and resizing.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the window event.
/// * `windows` - A query for all windows.
/// * `displays` - A query for the active display.
/// * `main_cid` - The main connection ID resource.
/// * `commands` - Bevy commands to spawn or despawn entities.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn dispatch_application_messages(
    trigger: On<WMEventTrigger>,
    windows: Windows,
    applications: Query<(&Application, &Children)>,
    unmanaged_query: Query<&Unmanaged>,
    mut commands: Commands,
) {
    let find_window = |window_id| windows.find(window_id);

    match &trigger.event().0 {
        Event::WindowMinimized { window_id } => {
            if let Some((_, entity)) = find_window(*window_id) {
                commands.entity(entity).try_insert(Unmanaged::Minimized);
            }
        }

        Event::WindowDeminimized { window_id } => {
            if let Some((_, entity)) = find_window(*window_id)
                && matches!(unmanaged_query.get(entity), Ok(Unmanaged::Minimized))
            {
                commands.entity(entity).try_remove::<Unmanaged>();
            }
        }

        Event::ApplicationHidden { pid } => {
            let Some((_, children)) = applications.iter().find(|(app, _)| app.pid() == *pid) else {
                warn!("Unable to find with pid {pid}");
                return;
            };
            for entity in children {
                // Only hide windows that are currently managed (no Unmanaged component).
                // Preserve existing Floating, Minimized, and Hidden states.
                if unmanaged_query.get(*entity).is_err() {
                    commands.entity(*entity).try_insert(Unmanaged::Hidden);
                }
            }
        }

        Event::ApplicationVisible { pid } => {
            let Some((_, children)) = applications.iter().find(|(app, _)| app.pid() == *pid) else {
                warn!("Unable to find application with pid {pid}");
                return;
            };
            for entity in children {
                // Only restore windows that were hidden by the app hide/show cycle.
                // Preserve Floating and Minimized states.
                if matches!(unmanaged_query.get(*entity), Ok(Unmanaged::Hidden)) {
                    commands.entity(*entity).try_remove::<Unmanaged>();
                }
            }
        }
        _ => (),
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn window_unmanaged_trigger(
    trigger: On<Add, Unmanaged>,
    windows: Windows,
    apps: Query<(Entity, &Application)>,
    mut workspaces: Query<&mut LayoutStrip>,
    active_display: Single<(&Display, Option<&DockPosition>), With<ActiveDisplayMarker>>,
    config: Configuration,
    mut commands: Commands,
) {
    const UNMANAGED_MAX_SCREEN_RATIO_NUM: i32 = 4;
    const UNMANAGED_MAX_SCREEN_RATIO_DEN: i32 = 5;
    const UNMANAGED_POP_OFFSET: i32 = 32;

    fn clamp_origin_to_bounds(origin: IRect, size: Size, bounds: IRect) -> IRect {
        let max = (bounds.max - size).max(bounds.min);
        let min = origin.min.clamp(bounds.min, max);
        IRect::from_corners(min, min + size)
    }

    fn offset_frame_within_bounds(frame: IRect, bounds: IRect, offset: i32) -> IRect {
        let candidates = [
            (offset, offset),
            (offset, -offset),
            (-offset, offset),
            (-offset, -offset),
            (offset, 0),
            (-offset, 0),
            (0, offset),
            (0, -offset),
        ];

        for (dx, dy) in candidates {
            let moved = IRect::from_corners(
                Origin::new(frame.min.x + dx, frame.min.y + dy),
                Origin::new(frame.max.x + dx, frame.max.y + dy),
            );
            if moved.min.x >= bounds.min.x
                && moved.max.x <= bounds.max.x
                && moved.min.y >= bounds.min.y
                && moved.max.y <= bounds.max.y
            {
                return moved;
            }
        }

        frame
    }

    let entity = trigger.event().entity;
    let Some((_, _, Some(Unmanaged::Floating))) = windows.get_managed(entity) else {
        return;
    };
    let display_bounds = {
        let (display, dock) = *active_display;
        display.actual_display_bounds(dock, config.config())
    };

    debug!("Entity {entity} is floating.");

    let Some((window, frame)) = windows.get(entity).zip(windows.frame(entity)) else {
        return;
    };
    let Some((_, app)) = windows
        .find_parent(window.id())
        .and_then(|(_, _, parent)| apps.get(parent).ok())
    else {
        return;
    };

    let properties = WindowProperties::new(app, window, config.config());

    // Skip the active-display reposition/resize during init; the strip
    // removal below still has to run.
    if !config.initializing() {
        if let Some((rx, ry, rw, rh)) = properties.grid_ratios() {
            let x = (f64::from(display_bounds.width()) * rx) as i32;
            let y = (f64::from(display_bounds.height()) * ry) as i32;
            let w = (f64::from(display_bounds.width()) * rw) as i32;
            let h = (f64::from(display_bounds.height()) * rh) as i32;
            reposition_entity(entity, Origin::new(x, y), &mut commands);
            resize_entity(entity, Size::new(w, h), &mut commands);
        } else {
            let max_width = display_bounds.width() * UNMANAGED_MAX_SCREEN_RATIO_NUM
                / UNMANAGED_MAX_SCREEN_RATIO_DEN;
            let max_height = display_bounds.height() * UNMANAGED_MAX_SCREEN_RATIO_NUM
                / UNMANAGED_MAX_SCREEN_RATIO_DEN;
            let new_width = frame.width().min(max_width);
            let new_height = frame.height().min(max_height);

            let mut target_frame =
                IRect::from_corners(frame.min, frame.min + Origin::new(new_width, new_height));
            target_frame =
                clamp_origin_to_bounds(target_frame, target_frame.size(), display_bounds);
            target_frame =
                offset_frame_within_bounds(target_frame, display_bounds, UNMANAGED_POP_OFFSET);

            if target_frame.size() != frame.size() {
                resize_entity(
                    entity,
                    Size::new(target_frame.width(), target_frame.height()),
                    &mut commands,
                );
            }
            if target_frame.min != frame.min {
                reposition_entity(entity, target_frame.min, &mut commands);
            }
        }
    }

    workspaces.par_iter_mut().for_each(|mut strip| {
        if strip.contains(entity) {
            strip.remove(entity);
        }
    });
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn window_minimized_trigger(
    trigger: On<Add, Unmanaged>,
    windows: Windows,
    workspaces: Query<(&mut LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    active_display: Single<&Display, With<ActiveDisplayMarker>>,
    mut config: Configuration,
    mut commands: Commands,
) {
    let entity = trigger.event().entity;
    if let Some((_, _, Some(Unmanaged::Minimized | Unmanaged::Hidden))) =
        windows.get_managed(entity)
    {
        debug!("Entity {entity} is minimized or hidden.");
        let display_bounds = active_display.bounds();

        for (mut strip, active) in workspaces {
            if active {
                give_away_focus(
                    entity,
                    &windows,
                    &strip,
                    &display_bounds,
                    &mut config,
                    &mut commands,
                );
            }
            if strip.contains(entity) {
                strip.remove(entity);
            }
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn window_managed_trigger(
    trigger: On<Remove, Unmanaged>,
    mut active_display: ActiveDisplayMut,
    windows: Windows,
    apps: Query<(Entity, &Application)>,
    config: Configuration,
    mut commands: Commands,
) {
    // finish_setup handles the initial strip assignment during init.
    if config.initializing() {
        return;
    }
    let entity = trigger.event().entity;

    debug!("Entity {entity} is managed again.");
    let display_bounds = active_display
        .display()
        .actual_display_bounds(active_display.dock(), config.config());
    let active_strip = active_display.active_strip();

    if let Some(window) = windows.get(entity)
        && let Some((_, app)) = windows
            .find_parent(window.id())
            .and_then(|(_, _, parent)| apps.get(parent).ok())
    {
        let properties = WindowProperties::new(app, window, config.config());

        if let Some(width_ratio) = properties.width_ratio() {
            let (_, pad_right, _, pad_left) = config.config().edge_padding();
            let padded_width = display_bounds.width() - pad_left - pad_right;
            let width = (f64::from(padded_width) * width_ratio).round() as i32;
            let height = display_bounds.height();
            resize_entity(entity, Size::new(width, height), &mut commands);
        }

        if properties.floating() {
            return;
        }
        if let Some(index) = properties.insertion() {
            active_strip.insert_at(index, entity);
            reshuffle_around(entity, &mut commands);
            return;
        }
    }

    active_strip.append(entity);
    reshuffle_around(entity, &mut commands);
}

/// Handles the event when a window is destroyed. The windows itself is not removed from the layout
/// strip. This happens in the On<Remove, Window> trigger.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the ID of the destroyed window.
/// * `windows` - A query for all windows with their parent.
/// * `apps` - A query for all applications.
/// * `displays` - A query for all displays.
/// * `commands` - Bevy commands to despawn entities and trigger events.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn window_destroyed_trigger(
    trigger: On<WMEventTrigger>,
    windows: Windows,
    active_display: ActiveDisplay,
    mut apps: Query<&mut Application>,
    mut config: Configuration,
    mut commands: Commands,
) {
    let Event::WindowDestroyed { window_id } = trigger.event().0 else {
        return;
    };

    let Some((window, entity, parent)) = windows.find_parent(window_id) else {
        debug!("Duplicate event: window {window_id} already destroyed.");
        return;
    };
    if window.role().is_ok() {
        debug!("Window still present, this was SLS workspace change.");
        return;
    }

    let Ok(mut app) = apps.get_mut(parent) else {
        error!("Window {} has no parent!", window.id());
        return;
    };
    app.unobserve_window(window);

    give_away_focus(
        entity,
        &windows,
        active_display.active_strip(),
        &active_display.bounds(),
        &mut config,
        &mut commands,
    );

    // NOTE: If the entity had an Unmanaged marker, despawning it will cause it to be re-inserted
    // into the strip again. Therefore we do it just before despawning the entity itself, so it
    // then can be properly removed again in the main entity despawn trigger.
    commands.entity(entity).remove::<Unmanaged>().despawn();

    // The window entity will be removed from the layout strip in the On<Remove> trigger.
}

/// Moves the focus away to a neighbour window.
fn give_away_focus(
    entity: Entity,
    windows: &Windows,
    active_strip: &LayoutStrip,
    viewport: &IRect,
    config: &mut Configuration,
    commands: &mut Commands,
) {
    if active_strip.tabbed(entity) {
        // Do not give away focus for tabbed windows.
        // Remaining tab gets the focus.
        return;
    }
    let display_center = viewport.center().x;
    let closest = active_strip
        .all_columns()
        .into_iter()
        .filter_map(|candidate| {
            if candidate == entity {
                return None;
            }
            let center = windows.moving_frame(candidate)?.center().x;
            let distance = (center - display_center).abs();
            Some((candidate, distance))
        })
        .min_by_key(|(_, dist)| *dist);

    if let Some((neighbour, _)) = closest
        && let Some(window) = windows.get(neighbour)
    {
        let window_id = window.id();

        config.set_ffm_flag(None);
        commands.trigger(WMEventTrigger(Event::WindowFocused { window_id }));
        reshuffle_around(neighbour, commands);
    }
}

/// Handles the event when a new window is created. It adds the window to the manager and sets focus.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the new windows.
/// * `windows` - A query for all windows.
/// * `apps` - A query for all applications.
/// * `active_display` - A query for the active display.
/// * `main_cid` - The main connection ID resource.
/// * `commands` - Bevy commands to manage components and trigger events.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn spawn_window_trigger(
    mut trigger: On<SpawnWindowTrigger>,
    windows: Windows,
    mut apps: Query<(Entity, &mut Application)>,
    mut active_display: ActiveDisplayMut,
    config: Configuration,
    mut commands: Commands,
) {
    let new_windows = &mut trigger.event_mut().0;

    while let Some(mut window) = new_windows.pop() {
        let window_id = window.id();

        if windows.find(window_id).is_some() {
            continue;
        }

        let Ok(pid) = window.pid() else {
            trace!("Unable to get window pid for {window_id}");
            continue;
        };
        let Some((app_entity, mut app)) = apps.iter_mut().find(|(_, app)| app.pid() == pid) else {
            trace!("unable to find application with pid {pid}.");
            continue;
        };

        debug!(
            "created {} title: {} role: {} subrole: {} element: {}",
            window_id,
            window.title().unwrap_or_default(),
            window.role().unwrap_or_default(),
            window.subrole().unwrap_or_default(),
            window
                .element()
                .map(|element| format!("{element}"))
                .unwrap_or_default(),
        );

        if app.observe_window(&window).is_err() {
            warn!("Error observing window {window_id}.");
        }
        debug!(
            "window {} title: {}",
            window_id,
            window.title().unwrap_or_default()
        );

        let properties = WindowProperties::new(&app, &window, config.config());
        if !properties.params.is_empty() {
            debug!("Applying window properties for '{}'", window.id());
        }

        apply_window_defaults(
            &mut window,
            &mut active_display,
            &properties.params,
            &config,
        );

        // update_frame expands the OS rect by the per-window padding, so calling it *after*
        // set_padding produces the correct logical frame for the ECS components below.
        let Ok(frame) = window.update_frame().inspect_err(|err| error!("{err}")) else {
            continue;
        };
        let position = Position(frame.min);
        let bounds = Bounds(Size::new(frame.width(), frame.height()));
        let width_ratio =
            WidthRatio(f64::from(frame.width()) / f64::from(active_display.bounds().width()));
        let layout_position = LayoutPosition::default();

        // Overlapping Frame Strategy: check if this window overlaps exactly with an existing
        // window from the same application. If so, it's likely a native tab.
        let tabbed_entity = windows
            .all_iter()
            .find_map(|(existing_window, entity, parent)| {
                (parent.parent() == app_entity && existing_window.frame() == window.frame())
                    .then_some(entity)
            });

        // Insert the window into the internal Bevy state.
        // This insertion triggers window attributes observer.
        let entity = commands
            .spawn((
                position,
                bounds,
                width_ratio,
                window,
                layout_position,
                ChildOf(app_entity),
            ))
            .id();

        if let Some(leader) = tabbed_entity {
            debug!(
                "Adding window {window_id} as a tab follower for leader {leader:?} (overlapping frame)"
            );
            let layout_strip = active_display.active_strip();
            _ = layout_strip
                .convert_to_tabs(leader, entity)
                .inspect_err(|err| error!("Failed to convert to tabs: {err}"));
        }
    }
}

#[allow(clippy::cast_possible_truncation)]
fn apply_window_defaults(
    window: &mut Window,
    active_display: &mut ActiveDisplayMut,
    properties: &[WindowParams],
    config: &Configuration,
) {
    let floating = properties
        .iter()
        .find_map(|props| props.floating)
        .unwrap_or(false);

    // Do not add padding to floating windows.
    if let Some(padding) = properties.iter().find_map(|props| props.vertical_padding)
        && !floating
    {
        window.set_padding(WindowPadding::Vertical(padding.clamp(0, 50)));
    }
    if let Some(padding) = properties.iter().find_map(|props| props.horizontal_padding)
        && !floating
    {
        window.set_padding(WindowPadding::Horizontal(padding.clamp(0, 50)));
    }
    if floating {
        // Skip grid_ratios during init: we don't know this window's display.
        if !config.initializing()
            && let Some((rx, ry, rw, rh)) = properties.iter().find_map(WindowParams::grid_ratios)
        {
            let bounds = active_display.bounds();
            let x = (f64::from(bounds.width()) * rx) as i32;
            let y = (f64::from(bounds.height()) * ry) as i32;
            let w = (f64::from(bounds.width()) * rw) as i32;
            let h = (f64::from(bounds.height()) * rh) as i32;
            window.reposition(Origin::new(x, y));
            window.resize(Size::new(w, h));
        }
        return;
    }

    // Apply configured width AFTER update_frame so it isn't overwritten.
    // Use padded display width (matching window_resize command behavior).
    // Safe during init: this only resizes, it doesn't reposition, so a
    // window on an inactive display stays put.
    if let Some(width) = properties.iter().find_map(|props| props.width) {
        _ = window.update_frame().inspect_err(|err| error!("{err}"));
        let bounds = active_display.bounds();
        let (_, pad_right, _, pad_left) = config.config().edge_padding();
        let padded_width = bounds.width() - pad_left - pad_right;
        let new_width = (f64::from(padded_width) * width).round() as i32;
        let height = window.frame().height();
        window.resize(Size::new(new_width, height));
        // Re-read the actual OS size: the app may enforce a minimum width
        // that differs from our request.
        _ = window.update_frame().inspect_err(|err| error!("{err}"));
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn apply_window_properties(
    trigger: On<Add, Window>,
    mut active_display: ActiveDisplayMut,
    windows: Windows,
    apps: Query<&Application>,
    config: Configuration,
    mut commands: Commands,
) {
    let entity = trigger.event().entity;

    if active_display.active_strip().tabbed(entity) {
        debug!("Ignoring tabbed {entity} attributes.");
        return;
    }

    let Some((window, _, parent)) = windows
        .get(entity)
        .and_then(|window| windows.find_parent(window.id()))
    else {
        return;
    };
    let Ok(app) = apps.get(parent) else {
        return;
    };
    let properties = WindowProperties::new(app, window, config.config());

    if properties.floating() {
        // Avoid managing window if it's floating.
        commands.entity(entity).try_insert(Unmanaged::Floating);
        return;
    }

    let strip = active_display.active_strip();

    // Attempt inserting the window at a pre-defined position.
    let insert_at = properties.insertion().map_or_else(
        || {
            // Otherwise attempt inserting it after the current focus.
            let focused_window = windows.focused();
            // Insert to the right of the currently focused window
            focused_window
                .and_then(|(_, entity)| strip.index_of(entity).ok())
                .and_then(|insert_at| (insert_at + 1 < strip.len()).then_some(insert_at + 1))
        },
        Some,
    );

    debug!("New window adding at {strip}");
    match insert_at {
        Some(after) => {
            debug!("New window inserted at {after}");
            strip.insert_at(after, entity);
        }
        None => strip.append(entity),
    }

    // During init, skip per-window reshuffles. finish_setup does a single
    // reshuffle after all windows are added.
    if !config.initializing()
        && properties.dont_focus()
        && let Some((focus, entity)) = windows.focused()
    {
        debug!(
            "Not focusing new window {entity}, keeping focus on '{}'",
            focus.title().unwrap_or_default()
        );
        focus_entity(entity, true, &mut commands);
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn refresh_configuration_trigger(
    trigger: On<WMEventTrigger>,
    window_manager: Res<WindowManager>,
    mut config: ResMut<Config>,
    watcher: Option<NonSendMut<Box<dyn Watcher>>>,
    windows: Windows,
    mut displays: Query<&mut Display>,
    applications: Query<&Application>,
) {
    let Event::ConfigRefresh(event) = &trigger.event().0 else {
        return;
    };
    let Some(mut watcher) = watcher else {
        return;
    };

    match &event.kind {
        EventKind::Modify(
            // When using the RecommendedWatcher, the event triggers on file data.
            // When using PollWatcher, it triggers on modification time.
            ModifyKind::Metadata(MetadataKind::WriteTime) | ModifyKind::Data(DataChange::Content),
        ) => (),
        EventKind::Remove(_) => {
            for path in &event.paths {
                _ = watcher.unwatch(path).inspect_err(|err| {
                    error!("unwatching the config '{}': {err}", path.display());
                });
            }
            return;
        }
        _ => return,
    }

    for path in &event.paths {
        if let Some(symlink) = symlink_target(path) {
            debug!(
                "symlink '{}' changed, replacing the watcher.",
                symlink.display()
            );
            if let Ok(new_watcher) = window_manager
                .setup_config_watcher(path)
                .inspect_err(|err| {
                    error!("watching the config '{}': {err}", path.display());
                })
            {
                *watcher = new_watcher;
            }
        }
        info!("Reloading configuration file; {}", path.display());
        _ = config.reload_config(path.as_path()).inspect_err(|err| {
            error!("loading config '{}': {err}", path.display());
        });
    }

    let height = config.menubar_height();
    for mut display in &mut displays {
        display.set_menubar_height_override(height);
    }

    // Recompute passthrough keys for the currently focused window.
    if let Some((window, _, parent)) = windows
        .focused()
        .and_then(|(w, e)| windows.find_parent(w.id()).map(|(w, _, p)| (w, e, p)))
        && let Ok(app) = applications.get(parent)
    {
        update_passthrough(window, app, &config);
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

#[allow(clippy::needless_pass_by_value)]
pub(super) fn window_removal_trigger(
    trigger: On<Remove, Window>,
    mut workspaces: Query<&mut LayoutStrip>,
) {
    let entity = trigger.event().entity;

    if let Some(mut strip) = workspaces.iter_mut().find(|strip| strip.contains(entity)) {
        debug!(
            "Removing despawned entity {entity} from strip {}",
            strip.id()
        );
        strip.remove(entity);
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn locate_dock_trigger(
    trigger: On<LocateDockTrigger>,
    displays: Query<(&mut Display, Entity)>,
    platform: Option<NonSend<Pin<Box<PlatformCallbacks>>>>,
    mut commands: Commands,
) {
    let Ok((display, entity)) = displays.get(trigger.event().0) else {
        return;
    };
    let display_id = display.id();

    // NSScreen::screen needs to run in the main thread, thus we run it in a NonSend trigger.
    let screens = platform.map(|platform| NSScreen::screens(platform.main_thread_marker));
    let dock = screens.as_ref().and_then(|screens| {
        screens.iter().find_map(|screen| {
            let dict = screen.deviceDescription();
            let numbers = unsafe { dict.cast_unchecked::<NSString, NSNumber>() };
            let id = numbers.objectForKey(ns_string!("NSScreenNumber"));
            id.is_some_and(|id| id.as_u32() == display_id).then(|| {
                let visible_frame = irect_from(screen.visibleFrame());
                display.locate_dock(&visible_frame)
            })
        })
    });
    if let Some(dock) = dock {
        debug!("dock on display {display_id}: {:?}", dock);
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_insert(dock);
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn send_message_trigger(
    trigger: On<SendMessageTrigger>,
    mut messages: MessageWriter<Event>,
) {
    let event = &trigger.event().0;
    messages.write(event.clone());
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn restore_window_state(
    _: On<RestoreWindowState>,
    windows: Windows,
    mut workspaces: Query<(&mut LayoutStrip, Has<ActiveWorkspaceMarker>, &ChildOf)>,
    displays: Query<(Entity, &Display)>,
    apps: Query<&Application>,
    restoration: Option<Res<PaneruState>>,
    mut commands: Commands,
) {
    let Some(restore) = restoration else {
        return;
    };

    let mut new_strips: Vec<(LayoutStrip, Entity)> = Vec::new();
    for (mut strip, _, child) in &mut workspaces {
        let restore = strip
            .all_windows()
            .into_iter()
            .filter_map(|entity| {
                let window = windows.get(entity)?;
                let bundle_id = windows
                    .find_parent(window.id())
                    .and_then(|(_, _, parent)| apps.get(parent).ok())
                    .map(|app| app.bundle_id().unwrap_or_default().to_string())?;
                Some(entity).zip(restore.match_window(window, &bundle_id))
            })
            .collect::<Vec<_>>();

        for index in 0u32..restore.len() as u32 {
            let mut entities = restore
                .iter()
                .filter(|(_, (_, previous_virt_id, _))| *previous_virt_id == index)
                .collect::<Vec<_>>();
            if entities.is_empty() {
                break;
            }
            entities.sort_by_key(|(_, (_, _, previous_idx))| *previous_idx);

            if index == 0 {
                let mut last_idx = 1000;

                for (entity, (_, _, previous_idx)) in entities {
                    strip.remove(*entity);

                    if last_idx == *previous_idx {
                        debug!("Stacking window {entity}");
                        strip.insert_at(last_idx + 1, *entity);
                        _ = strip.stack(*entity);
                    } else {
                        strip.insert_at(*previous_idx, *entity);
                    }
                    last_idx = *previous_idx;
                }
            } else {
                let mut new_strip = new_strips.iter_mut().find_map(|(new_strip, _)| {
                    (new_strip.id() == strip.id() && new_strip.virtual_index == index)
                        .then_some(new_strip)
                });
                if new_strip.is_none() {
                    debug!("Creating new virtual strip {index}");
                    new_strips.push((LayoutStrip::new(strip.id(), index), child.parent()));
                    new_strip = new_strips.last_mut().map(|(strip, _)| strip);
                }

                if let Some(new_strip) = new_strip {
                    let mut last_idx = 1000;

                    for (entity, (_, _, previous_idx)) in entities {
                        strip.remove(*entity);
                        debug!("Inserting {} into new strip", *entity);

                        if last_idx == *previous_idx {
                            debug!("Stacking window {entity}");
                            new_strip.insert_at(last_idx + 1, *entity);
                            _ = new_strip.stack(*entity);
                        } else {
                            new_strip.insert_at(*previous_idx, *entity);
                        }
                        last_idx = *previous_idx;
                    }
                }
            }
        }
    }

    for (strip, parent) in new_strips {
        let Ok(bounds) = displays.get(parent).map(|(_, display)| display.bounds()) else {
            continue;
        };

        let previous = PreviousStripPosition {
            origin: bounds.min,
            focus: strip.all_windows().first().copied(),
        };
        let hidden_origin = Position(bounds.max - 10);
        commands.spawn((strip, hidden_origin, previous, ChildOf(parent)));
    }
}

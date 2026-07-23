use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::{ChildOf, Children};
use bevy::ecs::lifecycle::{Add, Remove, RemovedComponents};
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::observer::On;
use bevy::ecs::query::{Added, Has, With};
use bevy::ecs::system::{Commands, NonSendMut, Populated, Query, Res, ResMut, Single};
use bevy::math::IRect;
use notify::event::{DataChange, MetadataKind, ModifyKind};
use notify::{EventKind, Watcher};
use std::cmp::Ordering;
use std::time::Duration;
use tracing::{Level, debug, error, info, instrument, trace, warn};

use super::{
    ActiveDisplayMarker, BProcess, FocusedMarker, FreshMarker, MissionControlActive,
    PreviousManagedStrip, RetryFrontSwitch, SpawnWindowTrigger, StrayFocusEvent, SystemTheme,
    Timeout, Unmanaged,
};
use crate::config::Config;
use crate::ecs::focus::FocusHistory;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{ActiveDisplay, GlobalState, Windows};
use crate::ecs::state::PaneruState;
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, DockPosition, Initializing, LayoutPosition, Position,
    ResizeMarker, RestoreWindowState, Scrolling, SendMessageTrigger, SpawnCommandsExt,
    VerifyWindowPosition, WidthRatio, WindowProperties,
};
use crate::events::Event;
use crate::manager::{
    Application, Display, Origin, Process, Size, Window, WindowManager, WindowPadding,
};
use crate::platform::WinID;
use crate::util::symlink_target;

/// Computes the passthrough keybinding set for the given window/app and
/// publishes it to the input thread. Called on focus change and config reload.
fn update_passthrough(window: &Window, app: &Application, config: &Config) {
    let properties = WindowProperties::new(app, window, config);
    crate::platform::input::set_focused_passthrough(properties.passthrough_keys());
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
    mut messages: MessageReader<Event>,
    processes: Query<(&BProcess, &Children)>,
    applications: Query<&Application>,
    window_manager: Res<WindowManager>,
    mut config: GlobalState,
    mut commands: Commands,
) {
    const FRONT_SWITCH_RETRY_SEC: u64 = 2;
    for event in messages.read() {
        let Event::ApplicationFrontSwitched { psn } = event else {
            continue;
        };

        let Some((BProcess(process), children)) =
            processes.iter().find(|process| &process.0.psn() == psn)
        else {
            error!("Unable to find process with PSN {psn:?}");
            continue;
        };

        if children.len() > 1 {
            warn!("Multiple apps registered to process '{}'.", process.name());
        }
        let Some(&app_entity) = children.first() else {
            error!("No application for process '{}'.", process.name());
            continue;
        };
        let Some(app) = applications.get(app_entity).ok() else {
            error!("No application for process '{}'.", process.name());
            continue;
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
            commands.trigger(SendMessageTrigger(Event::WindowFocused {
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
                &mut commands,
            );
            commands.spawn((timeout, RetryFrontSwitch(app_entity)));
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn theme_change_trigger(
    mut messages: MessageReader<Event>,
    windows: Windows,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut theme: Option<ResMut<SystemTheme>>,
) {
    for event in messages.read() {
        let Event::ThemeChanged = event else {
            continue;
        };

        let Some(ref mut theme) = theme else {
            continue;
        };

        let is_dark = crate::util::is_dark_mode();
        if theme.is_dark == is_dark {
            continue;
        }
        theme.is_dark = is_dark;
        info!("System theme changed: dark_mode={is_dark}");

        let Some(dim_ratio) = config.window_dim_ratio(is_dark) else {
            continue;
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
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn window_focused_trigger(
    mut messages: MessageReader<Event>,
    applications: Query<&Application>,
    windows: Windows,
    mut workspaces: Query<(Entity, &mut LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    mut focus_history: ResMut<FocusHistory>,
    config: Res<Config>,
    global_state: GlobalState,
    mut commands: Commands,
) {
    const STRAY_FOCUS_RETRY_SEC: u64 = 2;

    for event in messages.read() {
        let Event::WindowFocused { window_id } = *event else {
            continue;
        };

        let Some((window, entity, parent)) = windows.find_parent(window_id) else {
            let timeout = Timeout::new(
                Duration::from_secs(STRAY_FOCUS_RETRY_SEC),
                None,
                &mut commands,
            );
            commands.spawn((timeout, StrayFocusEvent(window_id)));
            continue;
        };

        let Ok(app) = applications.get(parent) else {
            warn!("Unable to get parent for window {}.", window.id());
            continue;
        };

        // Always keep passthrough in sync. An internal focus_entity call races
        // with the OS WindowFocused event; without this the passthrough keys
        // remain stale from a previously focused window.
        update_passthrough(window, app, &config);

        let already_focused = windows
            .focused()
            .is_some_and(|(focused, _)| focused.id() == window_id);

        // Guard against stale focus events. Without these checks, delayed
        // events (e.g. from RetryFrontSwitch or dont_focus re-assertions)
        // can pull FocusedMarker back to an old window after focus has moved on.
        //
        // 1. Cross-app: skip if the window's app is no longer frontmost.
        // 2. Same-app: skip if the app's current focused window differs from
        //    this event's window_id (the event is outdated).
        if !app.is_frontmost() {
            continue;
        }
        if app.focused_window_id().is_ok_and(|id| id != window_id) {
            continue;
        }

        let managed = windows
            .get_managed(entity)
            .and_then(|(_, _, managed)| managed);
        if matches!(managed, Some(Unmanaged::Hidden)) {
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_remove::<Unmanaged>();
            }
            commands.trigger(SendMessageTrigger(Event::WindowFocused { window_id }));
            continue;
        }

        // Handle tab switching: if the focused window is a tab, make it the leader.
        // Also reactivate the owning virtual strip before treating duplicate
        // focus as a no-op; the focus marker can be stale on a hidden strip.
        // Track the active workspace as a fallback so focus_history can record
        // a workspace id even when the entity hasn't been routed into a strip.
        let mut owner = None;
        let mut owning_workspace_id = None;
        let mut active_workspace_id = None;
        for (strip_entity, mut strip, active) in &mut workspaces {
            if active {
                active_workspace_id = Some(strip.id());
            }
            if owner.is_none() && strip.contains(entity) {
                if let Ok(index) = strip.index_of(entity)
                    && let Some(column) = strip.get_column_mut(index)
                {
                    column.move_to_front(entity);
                }
                owning_workspace_id = Some(strip.id());
                owner = Some((strip_entity, active));
            }
        }

        if owner.is_none() && managed.is_none() {
            // The window just spawned and has not yet been inserted into the strip.
            continue;
        }

        if let Some((strip_entity, active)) = owner
            && !active
            && let Ok(mut entity_commands) = commands.get_entity(strip_entity)
        {
            entity_commands.try_insert(ActiveWorkspaceMarker);
        }

        // Record before the already-focused short-circuit below: focus_entity
        // sets FocusedMarker synchronously, so OS-confirmed events for the
        // same entity would otherwise skip the write.
        if let Some(workspace_id) = owning_workspace_id.or(active_workspace_id) {
            let unmanaged = windows.get_managed(entity).and_then(|(_, _, u)| u);
            focus_history.record(workspace_id, entity, unmanaged);
        }

        if already_focused {
            if !global_state.skip_reshuffle() && !global_state.initializing() {
                commands.reshuffle_around(entity);
            }
            continue;
        }

        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_insert(FocusedMarker);
            debug!("window {} ({entity}) focused.", window.id());
        }
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
    mut messages: MessageReader<Event>,
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
    for event in messages.read() {
        match event {
            Event::MissionControlShowAllWindows
            | Event::MissionControlShowFrontWindows
            | Event::MissionControlShowDesktop => {
                mission_control_active.as_mut().0 = true;
                for (entity, _, _, scroll) in &workspaces {
                    if scroll.is_some()
                        && let Ok(mut entity_commands) = commands.get_entity(entity)
                    {
                        entity_commands.try_remove::<Scrolling>();
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
                    && let Ok(present_windows) =
                        window_manager.windows_in_workspace(active_strip.id())
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
    mut messages: MessageReader<Event>,
    processes: Query<(&BProcess, Entity)>,
    mut commands: Commands,
) {
    const PROCESS_READY_TIMEOUT_SEC: u64 = 5;
    let find_process = |psn| {
        processes
            .iter()
            .find(|(BProcess(process), _)| process.psn() == psn)
    };

    for event in messages.read() {
        match event {
            Event::ApplicationLaunched { psn, observer } if find_process(*psn).is_none() => {
                let process: BProcess = Process::new(psn, observer.clone()).into();
                let timeout = Timeout::new(
                    Duration::from_secs(PROCESS_READY_TIMEOUT_SEC),
                    Some(format!(
                        "Process '{}' did not become ready in {PROCESS_READY_TIMEOUT_SEC}s.",
                        process.name()
                    )),
                    &mut commands,
                );
                commands.spawn((FreshMarker, timeout, process));
            }

            Event::ApplicationTerminated { psn } => {
                if let Some((_, entity)) = find_process(*psn)
                    && let Ok(mut entity_commands) = commands.get_entity(entity)
                {
                    entity_commands.try_despawn();
                }
            }
            _ => (),
        }
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
    mut messages: MessageReader<Event>,
    windows: Windows,
    applications: Query<(&Application, &Children)>,
    unmanaged_query: Query<&Unmanaged>,
    mut commands: Commands,
) {
    let find_window = |window_id| windows.find(window_id);

    for event in messages.read() {
        match event {
            Event::WindowMinimized { window_id } => {
                if let Some((_, entity)) = find_window(*window_id)
                    && let Ok(mut entity_commands) = commands.get_entity(entity)
                {
                    entity_commands.try_insert(Unmanaged::Minimized);
                }
            }

            Event::WindowDeminimized { window_id } => {
                if let Some((_, entity)) = find_window(*window_id)
                    && matches!(unmanaged_query.get(entity), Ok(Unmanaged::Minimized))
                    && let Ok(mut entity_commands) = commands.get_entity(entity)
                {
                    entity_commands.try_remove::<Unmanaged>();
                }
            }

            Event::ApplicationHidden { pid } => {
                let Some((_, children)) = applications.iter().find(|(app, _)| app.pid() == *pid)
                else {
                    warn!("Unable to find with pid {pid}");
                    continue;
                };
                for entity in children {
                    // Only hide windows that are currently managed (no Unmanaged component).
                    // Preserve existing Floating, Minimized, and Hidden states.
                    if unmanaged_query.get(*entity).is_err()
                        && let Ok(mut entity_commands) = commands.get_entity(*entity)
                    {
                        entity_commands.try_insert(Unmanaged::Hidden);
                    }
                }
            }

            Event::ApplicationVisible { pid } => {
                let Some((_, children)) = applications.iter().find(|(app, _)| app.pid() == *pid)
                else {
                    warn!("Unable to find application with pid {pid}");
                    continue;
                };
                for entity in children {
                    // Only restore windows that were hidden by the app hide/show cycle.
                    // Preserve Floating and Minimized states.
                    if matches!(unmanaged_query.get(*entity), Ok(Unmanaged::Hidden))
                        && let Ok(mut entity_commands) = commands.get_entity(*entity)
                    {
                        entity_commands.try_remove::<Unmanaged>();
                    }
                }
            }
            _ => (),
        }
    }
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn window_unmanaged_trigger(
    trigger: On<Add, Unmanaged>,
    windows: Windows,
    apps: Query<(Entity, &Application)>,
    workspaces: Query<&mut LayoutStrip>,
    active_display: Single<(&Display, Option<&DockPosition>), With<ActiveDisplayMarker>>,
    config: Res<Config>,
    initializing: Option<Res<Initializing>>,
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
        display.actual_display_bounds(dock, &config)
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

    let properties = WindowProperties::new(app, window, &config);

    // Skip the active-display reposition/resize during init; the strip
    // removal below still has to run.
    if let Some((rx, ry, rw, rh)) = properties.grid_ratios() {
        let x = (f64::from(display_bounds.width()) * rx) as i32;
        let y = (f64::from(display_bounds.height()) * ry) as i32;
        let w = (f64::from(display_bounds.width()) * rw) as i32;
        let h = (f64::from(display_bounds.height()) * rh) as i32;
        commands.reposition_entity(entity, Origin::new(x, y));
        commands.resize_entity(entity, Size::new(w, h));
    } else if initializing.is_none() && !properties.floating() {
        let max_width = display_bounds.width() * UNMANAGED_MAX_SCREEN_RATIO_NUM
            / UNMANAGED_MAX_SCREEN_RATIO_DEN;
        let max_height = display_bounds.height() * UNMANAGED_MAX_SCREEN_RATIO_NUM
            / UNMANAGED_MAX_SCREEN_RATIO_DEN;
        let new_width = frame.width().min(max_width);
        let new_height = frame.height().min(max_height);

        let mut target_frame =
            IRect::from_corners(frame.min, frame.min + Origin::new(new_width, new_height));
        target_frame = clamp_origin_to_bounds(target_frame, target_frame.size(), display_bounds);
        target_frame =
            offset_frame_within_bounds(target_frame, display_bounds, UNMANAGED_POP_OFFSET);

        if target_frame.size() != frame.size() {
            commands.resize_entity(
                entity,
                Size::new(target_frame.width(), target_frame.height()),
            );
        }
        if target_frame.min != frame.min {
            commands.reposition_entity(entity, target_frame.min);
        }
    }

    workspaces.into_iter().for_each(|mut strip| {
        if strip.contains(entity) {
            remember_managed_strip(entity, &strip, &mut commands);
            strip.remove(entity);
        }
    });
}

fn remember_managed_strip(entity: Entity, strip: &LayoutStrip, commands: &mut Commands) {
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        entity_commands.try_insert(PreviousManagedStrip {
            workspace_id: strip.id(),
            virtual_index: strip.virtual_index,
            index: strip.index_of(entity).unwrap_or(strip.len()),
        });
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn window_minimized_trigger(
    trigger: On<Add, Unmanaged>,
    windows: Windows,
    workspaces: Query<(&mut LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    active_display: Single<&Display, With<ActiveDisplayMarker>>,
    mut config: GlobalState,
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
                remember_managed_strip(entity, &strip, &mut commands);
                strip.remove(entity);
            }
        }
    }
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn window_managed_trigger(
    trigger: On<Remove, Unmanaged>,
    active_display: Single<(&Display, Option<&DockPosition>), With<ActiveDisplayMarker>>,
    windows: Windows,
    apps: Query<(Entity, &Application)>,
    mut workspaces: Query<(&mut LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    previous_strips: Query<&PreviousManagedStrip>,
    config: Res<Config>,
    initializing: Option<Res<Initializing>>,
    mut commands: Commands,
) {
    // finish_setup handles the initial strip assignment during init.
    if initializing.is_some() {
        return;
    }
    let entity = trigger.event().entity;

    if windows
        .get(entity)
        .is_some_and(|window| window.role().is_err())
    {
        // The marker was removed because the windows was destroyed.
        return;
    }

    debug!("Entity {entity} is managed again.");
    let (display, dock) = *active_display;
    let display_bounds = display.actual_display_bounds(dock, &config);
    let mut insert_at = previous_strips
        .get(entity)
        .ok()
        .map(|previous| previous.index);

    if let Some(window) = windows.get(entity)
        && let Some((_, app)) = windows
            .find_parent(window.id())
            .and_then(|(_, _, parent)| apps.get(parent).ok())
    {
        let properties = WindowProperties::new(app, window, &config);

        if let Some(width_ratio) = properties.width_ratio() {
            let (_, pad_right, _, pad_left) = config.edge_padding();
            let padded_width = display_bounds.width() - pad_left - pad_right;
            let width = (f64::from(padded_width) * width_ratio).round() as i32;
            let height = display_bounds.height();
            commands.resize_entity(entity, Size::new(width, height));
        }

        insert_at = properties.insertion().or(insert_at);
    }

    let previous = previous_strips.get(entity).ok().copied();
    for (mut strip, _) in &mut workspaces {
        strip.remove(entity);
    }

    let mut restored = false;
    if let Some(previous) = previous {
        for (mut strip, _) in &mut workspaces {
            if strip.id() == previous.workspace_id && strip.virtual_index == previous.virtual_index
            {
                strip.insert_at(insert_at.unwrap_or(previous.index), entity);
                restored = true;
                break;
            }
        }
    }

    if !restored
        && let Some((mut active_strip, _)) = workspaces.iter_mut().find(|(_, active)| *active)
    {
        if let Some(index) = insert_at {
            active_strip.insert_at(index, entity);
        } else {
            // Insert at the column the floating window visually overlaps so the
            // strip doesn't have to scroll to the end to expose the new column.
            let insertion = windows.frame(entity).and_then(|frame| {
                let center_x = frame.center().x;
                active_strip.all_columns().into_iter().position(|top| {
                    windows
                        .frame(top)
                        .is_some_and(|col| col.center().x > center_x)
                })
            });
            let insertion = insertion.unwrap_or(active_strip.len());
            active_strip.insert_at(insertion, entity);
        }
    }

    if let Some(origin) = windows.origin(entity) {
        commands.reposition_entity(entity, origin);
    }
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        entity_commands
            .try_insert(VerifyWindowPosition::default())
            .try_remove::<PreviousManagedStrip>();
    }
    commands.reshuffle_around(entity);
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
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    mut apps: Query<&mut Application>,
    mut config: GlobalState,
    mut focus_history: ResMut<FocusHistory>,
    mut commands: Commands,
) {
    for event in messages.read() {
        let Event::WindowDestroyed { window_id } = event else {
            continue;
        };

        let Some((window, entity, parent)) = windows.find_parent(*window_id) else {
            debug!("Duplicate event: window {window_id} already destroyed.");
            continue;
        };
        if window.role().is_ok() {
            debug!("Window still present, this was SLS workspace change.");
            continue;
        }

        let Ok(mut app) = apps.get_mut(parent) else {
            error!("Window {} has no parent!", window.id());
            continue;
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
        focus_history.forget(entity);

        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_despawn();
        }

        // The window entity will be removed from the layout strip in the On<Remove> trigger.
    }
}

/// Moves the focus away to a neighbour window.
fn give_away_focus(
    entity: Entity,
    windows: &Windows,
    active_strip: &LayoutStrip,
    viewport: &IRect,
    config: &mut GlobalState,
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
        .filter(|&candidate| candidate != entity)
        .filter_map(|candidate| {
            let center = windows.moving_frame(candidate)?.center().x;
            let distance = (center - display_center).abs();
            Some((candidate, distance))
        })
        .min_by_key(|(_, dist)| *dist)
        .map(|(e, _)| e)
        .or_else(|| {
            // Fallback when no candidate has a usable frame: pick any other
            // column in the strip. Without this, losing focus on the only
            // geometrically-known window would leave FocusedMarker unset and
            // silently break keybindings.
            active_strip
                .all_columns()
                .into_iter()
                .find(|&candidate| candidate != entity)
        });

    if let Some(neighbour) = closest
        && windows.get(neighbour).is_some()
    {
        config.set_ffm_flag(None);
        // Use focus_entity instead of triggering Event::WindowFocused: the
        // OS has usually handed focus to a different app after the current
        // window closed/hid, so window_focused_trigger's frontmost/focused
        // guards would reject a fabricated event. focus_entity calls the
        // AX API to raise the neighbour and inserts FocusedMarker directly.
        commands.focus_entity(neighbour, true);
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
    windows: Query<&Window>,
    mut apps: Query<(Entity, &mut Application)>,
    active_display: ActiveDisplay,
    initializing: Option<Res<Initializing>>,
    restore: Option<Res<crate::ecs::restore::SessionRestore>>,
    mut commands: Commands,
) {
    let new_windows = &mut trigger.event_mut().0;

    while let Some(mut window) = new_windows.pop() {
        let window_id = window.id();

        if windows.iter().any(|window| window.id() == window_id) {
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

        if tracing::enabled!(Level::DEBUG) {
            let title = window.title().unwrap_or_default();
            let role = window.role().unwrap_or_default();
            let subrole = window.subrole().unwrap_or_default();
            let element = window
                .element()
                .map(|element| format!("{element}"))
                .unwrap_or_default();
            debug!(
                "created {window_id} title: {title} role: {role} subrole: {subrole} element: {element}",
            );
        }

        if app.observe_window(&window).is_err() {
            warn!("Error observing window {window_id}.");
        }

        // update_frame expands the OS rect by the per-window padding, so calling it *after*
        // set_padding produces the correct logical frame for the ECS components below.
        let Ok(frame) = window.update_frame().inspect_err(|err| error!("{err}")) else {
            continue;
        };
        let position = Position(frame.min);
        let bounds = Bounds(frame.size());
        let width_ratio =
            WidthRatio(f64::from(frame.width()) / f64::from(active_display.bounds().width()));
        let layout_position = LayoutPosition::default();

        // Insert the window into the internal Bevy state.
        // This insertion triggers window attributes observer.
        commands.spawn((
            position,
            bounds,
            width_ratio,
            window,
            layout_position,
            ChildOf(app_entity),
        ));
    }

    if initializing.is_none() && restore.is_some() {
        commands.trigger(RestoreWindowState);
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn apply_window_defaults(
    added: Populated<(&mut Window, &mut Position, &mut Bounds, &ChildOf), Added<Window>>,
    apps: Query<(Entity, &Application)>,
    active_display: ActiveDisplay,
    config: Res<Config>,
    initializing: Option<Res<Initializing>>,
) {
    for (ref mut window, mut position, mut bounds, child) in added {
        let Ok((_, app)) = apps.get(child.parent()) else {
            continue;
        };

        let properties = WindowProperties::new(app, window, &config);
        debug!("Applying window defaults for '{}'", window.id());

        let initializing = initializing.is_some();

        // Do not add padding to floating windows.
        if properties.floating() {
            // Skip grid_ratios during init: we don't know this window's display.
            if !initializing && let Some((rx, ry, rw, rh)) = properties.grid_ratios() {
                let bounds = active_display.actual_bounds(&config);
                let x = (f64::from(bounds.width()) * rx) as i32;
                let y = (f64::from(bounds.height()) * ry) as i32;
                let w = (f64::from(bounds.width()) * rw) as i32;
                let h = (f64::from(bounds.height()) * rh) as i32;
                window.reposition(Origin::new(x, y));
                window.resize(Size::new(w, h));
            }
            continue;
        }
        let vpadding = properties.vertical_padding();
        let hpadding = properties.horizontal_padding();
        window.set_padding(WindowPadding::Vertical(vpadding.clamp(0, 50)));
        window.set_padding(WindowPadding::Horizontal(hpadding.clamp(0, 50)));
        if let Ok(frame) = window.update_frame() {
            position.0 = frame.min;
            bounds.0 = frame.size();
        }

        // Apply configured width AFTER update_frame so it isn't overwritten.
        // Use padded display width (matching window_resize command behavior).
        // Safe during init: this only resizes, it doesn't reposition, so a
        // window on an inactive display stays put.
        if let Some(width) = properties.width_ratio() {
            _ = window.update_frame().inspect_err(|err| error!("{err}"));
            let bounds = active_display.actual_bounds(&config);
            let (_, pad_right, _, pad_left) = config.edge_padding();
            let padded_width = bounds.width() - pad_left - pad_right;
            let new_width = (f64::from(padded_width) * width).round() as i32;
            let height = window.frame().height();
            window.resize(Size::new(new_width, height));
            // Re-read the actual OS size: the app may enforce a minimum width
            // that differs from our request.
            _ = window.update_frame().inspect_err(|err| error!("{err}"));
        }
    }
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn apply_window_positions(
    added: Populated<Entity, Added<Window>>,
    mut workspaces: Query<(&mut LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    windows: Windows,
    apps: Query<&Application>,
    config: Res<Config>,
    initializing: Option<Res<Initializing>>,
    restore: Option<Res<crate::ecs::restore::SessionRestore>>,
    restoration: Option<Res<PaneruState>>,
    mut commands: Commands,
) {
    for entity in added {
        if workspaces.iter().any(|(strip, _)| strip.tabbed(entity)) {
            debug!("Ignoring tabbed {entity} attributes.");
            continue;
        }

        let Some((window, _, parent)) = windows
            .get(entity)
            .and_then(|window| windows.find_parent(window.id()))
        else {
            continue;
        };
        let Ok(app) = apps.get(parent) else {
            continue;
        };

        if crate::ecs::restore::matches_startup_restore_state(
            window,
            app,
            restore.as_deref(),
            restoration.as_deref(),
            &config,
        ) {
            continue;
        }

        let properties = WindowProperties::new(app, window, &config);

        if properties.floating() {
            let remembered = if let Some(mut strip) = workspaces
                .iter_mut()
                .find_map(|(strip, _)| strip.contains(entity).then_some(strip))
            {
                remember_managed_strip(entity, &strip, &mut commands);
                strip.remove(entity);
                true
            } else {
                false
            };
            if !remembered
                && let Some(strip) = workspaces
                    .iter()
                    .find_map(|(strip, active)| active.then_some(strip))
            {
                remember_managed_strip(entity, strip, &mut commands);
            }
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                // Avoid managing window if it's floating.
                entity_commands.try_insert(Unmanaged::Floating);
            }
            continue;
        }

        // During startup, the window is already inserted into some strip.
        let allready_inserted = workspaces
            .iter_mut()
            .find_map(|(strip, _)| strip.contains(entity).then_some(strip));
        if allready_inserted.is_none()
            && let Some(mut strip) = workspaces
                .iter_mut()
                .find_map(|(strip, active)| active.then_some(strip))
        {
            // Attempt inserting the window at a pre-defined position.
            let insert_at = properties.insertion().map_or_else(
                || {
                    // Otherwise attempt inserting it after the current focus.
                    let focused_window = windows.focused();
                    // Insert to the right of the currently focused window
                    focused_window
                        .and_then(|(_, entity)| strip.index_of(entity).ok())
                        .and_then(|insert_at| {
                            (insert_at + 1 < strip.len()).then_some(insert_at + 1)
                        })
                },
                Some,
            );

            debug!("New window {entity} adding at {}", *strip);
            match insert_at {
                Some(after) => {
                    debug!("New window inserted at {after}");
                    strip.insert_at(after, entity);
                }
                None => strip.append(entity),
            }
        }

        // During init, skip per-window reshuffles. finish_setup does a single
        // reshuffle after all windows are added.
        if initializing.is_none() {
            if properties.dont_focus() {
                if let Some((focus, prev)) = windows.focused() {
                    debug!(
                        "Not focusing new window {entity}, keeping focus on '{}'",
                        focus.title().unwrap_or_default()
                    );
                    commands.focus_entity(prev, true);
                }
            } else {
                debug!("Synthesizing WindowFocused for newly spawned window {entity}");
                commands.trigger(SendMessageTrigger(Event::WindowFocused {
                    window_id: window.id(),
                }));
            }
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn refresh_configuration_trigger(
    mut messages: MessageReader<Event>,
    window_manager: Res<WindowManager>,
    mut config: ResMut<Config>,
    mut watcher: Option<NonSendMut<Box<dyn Watcher>>>,
    windows: Windows,
    mut displays: Query<&mut Display>,
    applications: Query<&Application>,
) {
    for event in messages.read() {
        let Event::ConfigRefresh(event) = event else {
            continue;
        };

        let Some(ref mut watcher) = watcher else {
            continue;
        };

        match &event.kind {
            EventKind::Modify(
                // When using the RecommendedWatcher, the event triggers on file data.
                // When using PollWatcher, it triggers on modification time.
                ModifyKind::Metadata(MetadataKind::WriteTime)
                | ModifyKind::Data(DataChange::Content),
            ) => (),
            EventKind::Remove(_) => {
                for path in &event.paths {
                    _ = watcher.unwatch(path).inspect_err(|err| {
                        error!("unwatching the config '{}': {err}", path.display());
                    });
                }
                continue;
            }
            _ => continue,
        }

        for path in &event.paths {
            if let Some(symlink) = symlink_target(path) {
                debug!(
                    "symlink '{}' changed, replacing the watcher.",
                    symlink.display()
                );
                if let Ok(new_watcher) =
                    window_manager
                        .setup_config_watcher(path)
                        .inspect_err(|err| {
                            error!("watching the config '{}': {err}", path.display());
                        })
                {
                    **watcher = new_watcher;
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
pub(super) fn send_message_trigger(
    trigger: On<SendMessageTrigger>,
    mut messages: MessageWriter<Event>,
) {
    let event = &trigger.event().0;
    messages.write(event.clone());
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn cleanup_timeout_trigger(
    trigger: On<Remove, Timeout>,
    all_timeouts: Query<&Timeout>,
    mut commands: Commands,
) {
    if let Ok(timeout) = all_timeouts.get(trigger.entity)
        && let Some(system_id) = timeout.system_id
    {
        commands.unregister_system(system_id);
    }
}

pub(super) fn window_resize_verifier(
    mut removed: RemovedComponents<ResizeMarker>,
    mut windows: Query<(&mut Window, &Position, &mut Bounds)>,
    layout_strips: Query<&LayoutStrip>,
    mut commands: Commands,
) {
    use std::cmp::Ordering;
    for entity in removed.read() {
        let Ok((mut window, _, mut bounds)) = windows.get_mut(entity) else {
            continue;
        };
        let Ok(frame) = window.update_frame() else {
            continue;
        };

        let actual_size = frame.size();
        let expected_size = bounds.0;

        // note: macOS loves to make window sizes to even numbers, so we treat actual+1 as equal.
        let width_ord = fuzzy_equal(actual_size.x, expected_size.x);
        let height_ord = fuzzy_equal(actual_size.y, expected_size.y);

        if width_ord == Ordering::Equal && height_ord == Ordering::Equal {
            continue;
        }
        debug!(
            "window '{}'({}) did not fully resized to {}, was {} instead",
            window.title().unwrap_or_default(),
            window.id(),
            expected_size,
            actual_size,
        );
        bounds.0 = actual_size;

        // we may hitting minimum width constraint on this window or this window isn't resizable.
        // if this window is a part of a column, other windows in the column might have resized(shrunk) successfully,
        // which leaves an empty space next to those windows.
        // try to expand those windows to fill the empty space. (it's free real estate after all)
        //
        // note that those windows might have max window constraints or isn't resiable.
        // so we need to ignore cases where windows are failing to expand to the target.
        if width_ord == Ordering::Less {
            let Some(column) = layout_strips.iter().find_map(|strip| {
                strip
                    .index_of(entity)
                    .ok()
                    .and_then(|idx| strip.get(idx).ok())
            }) else {
                continue;
            };

            let get_window_frame = |entity| {
                windows
                    .get(entity)
                    .map(|(_, position, bounds)| {
                        IRect::from_corners(position.0, position.0 + bounds.0)
                    })
                    .ok()
            };

            let Some(column_width) = column.width(&get_window_frame) else {
                continue;
            };

            column
                .window_iter()
                .filter(|e| *e != entity)
                .for_each(|entity| {
                    if let Some(width) = get_window_frame(entity).as_ref().map(IRect::width)
                        && width < column_width
                    {
                        commands.resize_entity(entity, Size::new(column_width, actual_size.y));
                    }
                });
        }
    }
}

fn fuzzy_equal<N>(actual_size: N, expected_size: N) -> Ordering
where
    N: std::ops::Sub<Output = N> + Ord + From<i8>,
{
    let diff = expected_size - actual_size;

    if diff < N::from(-1) {
        Ordering::Less
    } else if diff > N::from(1) {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

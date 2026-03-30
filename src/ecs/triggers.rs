use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::{ChildOf, Children};
use bevy::ecs::lifecycle::{Add, Remove};
use bevy::ecs::message::MessageWriter;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Has, With};
use bevy::ecs::system::{Commands, NonSend, NonSendMut, Populated, Query, Res, ResMut};
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
    MouseHeldMarker, NativeFullscreenMarker, RetryFrontSwitch, SpawnWindowTrigger, StrayFocusEvent,
    SystemTheme, Timeout, Unmanaged, WMEventTrigger, WindowDraggedMarker,
};
use crate::config::{Config, WindowParams};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{ActiveDisplay, ActiveDisplayMut, Configuration, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, LayoutPosition, LocateDockTrigger, Position, Scrolling,
    SendMessageTrigger, WidthRatio, WindowProperties, reposition_entity, reshuffle_around,
    resize_entity,
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

/// Handles mouse moved events.
///
/// If "focus follows mouse" is enabled, this function finds the window under the cursor and
/// focuses it. It also handles child windows like sheets and drawers to ensure the correct
/// window receives focus.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the mouse moved event.
/// * `windows` - A query for all windows.
/// * `focused_window` - A query for the currently focused window.
/// * `main_cid` - The main connection ID resource.
/// * `config` - The optional configuration resource.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn mouse_moved_trigger(
    trigger: On<WMEventTrigger>,
    windows: Windows,
    apps: Query<&Application>,
    window_manager: Res<WindowManager>,
    mut config: Configuration,
) {
    let Event::MouseMoved { point } = trigger.event().0 else {
        return;
    };

    if !config.focus_follows_mouse() {
        return;
    }
    if config.mission_control_active() {
        return;
    }
    if config.ffm_flag().is_some() {
        trace!("ffm_window_id > 0");
        return;
    }
    let Ok(window_id) = window_manager.find_window_at_point(&point) else {
        debug!("can not find window at point {point:?}");
        return;
    };
    if windows
        .focused()
        .is_some_and(|(window, _)| window.id() == window_id)
    {
        trace!("allready focused {window_id}");
        return;
    }
    let Some((window, _)) = windows.find(window_id) else {
        trace!("can not find focused window: {window_id}");
        return;
    };

    let child_window = window_manager
        .get_associated_windows(window_id)
        .into_iter()
        .find_map(|child_wid| {
            windows.find(child_wid).and_then(|(window, _)| {
                window
                    .child_role()
                    .inspect_err(|err| {
                        warn!("getting role {window_id}: {err}");
                    })
                    .is_ok_and(|child| child)
                    .then_some(window)
            })
        });
    if let Some(child) = child_window {
        debug!("found child of {}: {}", child.id(), window.id());
    }

    // Do not reshuffle windows due to moved mouse focus.
    config.set_skip_reshuffle(true);
    config.set_ffm_flag(Some(window.id()));

    if let Some(psn) = windows.psn(window.id(), &apps) {
        if let Some((focused_window, _)) = windows.focused()
            && let Some(focused_psn) = windows.psn(focused_window.id(), &apps)
        {
            window.focus_without_raise(psn, focused_window, focused_psn);
        } else {
            window.focus_with_raise(psn);
        }
    }
}

/// Handles mouse down events.
///
/// This function finds the window at the click point. If the window is not fully visible,
/// it triggers a reshuffle to expose it.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the mouse down event.
/// * `windows` - A query for all windows.
/// * `active_display` - A query for the active display.
/// * `main_cid` - The main connection ID resource.
/// * `mission_control_active` - A resource indicating if Mission Control is active.
/// * `commands` - Bevy commands to trigger a reshuffle.
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
pub(super) fn mouse_down_trigger(
    trigger: On<WMEventTrigger>,
    windows: Windows,
    active_workspace: Query<(Entity, Option<&Scrolling>), With<ActiveWorkspaceMarker>>,
    window_manager: Res<WindowManager>,
    mission_control_active: Res<MissionControlActive>,
    config: Configuration,
    mouse_held: Query<Entity, With<MouseHeldMarker>>,
    mut commands: Commands,
) {
    let Event::MouseDown { point } = trigger.event().0 else {
        return;
    };
    if mission_control_active.0 {
        return;
    }
    trace!("{point:?}");

    let Some((_, entity)) = window_manager
        .find_window_at_point(&point)
        .ok()
        .and_then(|window_id| windows.find(window_id))
    else {
        return;
    };

    // Stop any ongoing scroll.
    for (entity, scroll) in active_workspace {
        if scroll.is_some() {
            commands.entity(entity).try_remove::<Scrolling>();
        }
    }

    // Clean up any stale marker from a previous click.
    for held in &mouse_held {
        commands.entity(held).despawn();
    }

    if config.window_hidden_ratio() >= 1.0 {
        // At max hidden ratio, never reshuffle on click.
    } else {
        // Defer reshuffle until mouse-up so the window doesn't shift
        // mid-click. The Timeout auto-despawns if mouse-up is lost.
        let timeout = Timeout::new(Duration::from_secs(5), None);
        commands.spawn((MouseHeldMarker(entity), timeout));
    }
}

/// Handles mouse-up events. Triggers the deferred reshuffle so the clicked
/// window slides into view after the user releases the button.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn mouse_up_trigger(
    trigger: On<WMEventTrigger>,
    mouse_held: Query<(Entity, &MouseHeldMarker)>,
    mut commands: Commands,
) {
    let Event::MouseUp { .. } = trigger.event().0 else {
        return;
    };
    for (held_entity, marker) in &mouse_held {
        reshuffle_around(marker.0, &mut commands);
        commands.entity(held_entity).despawn();
    }
}

/// Handles mouse dragged events.
///
/// This function is currently a placeholder and only logs the drag event.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the mouse dragged event.
/// * `mission_control_active` - A resource indicating if Mission Control is active.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn mouse_dragged_trigger(
    trigger: On<WMEventTrigger>,
    active_display: ActiveDisplay,
    windows: Windows,
    mut drag_marker: Query<(&mut Timeout, &mut WindowDraggedMarker)>,
    window_manager: Res<WindowManager>,
    mission_control_active: Res<MissionControlActive>,
    mut commands: Commands,
) {
    const DRAG_MARKER_TIMEOUT_MS: u64 = 1000;
    let Event::MouseDragged { point } = trigger.event().0 else {
        return;
    };
    if mission_control_active.0 {
        return;
    }

    let Some((window, entity)) = window_manager
        .0
        .find_window_at_point(&point)
        .ok()
        .and_then(|window_id| windows.find(window_id))
    else {
        return;
    };

    if let Ok((mut timeout, mut marker)) = drag_marker.single_mut() {
        // Change the current marker contents and refresh the timer.
        if entity != marker.entity {
            let marker = marker.as_mut();
            marker.entity = entity;
            marker.display_id = active_display.id();
            timeout.timer.reset();
        }
    } else {
        debug!(
            "Adding a drag marker ({entity}, {}) to window id {}.",
            active_display.id(),
            window.id(),
        );
        let timeout = Timeout::new(Duration::from_millis(DRAG_MARKER_TIMEOUT_MS), None);
        commands.spawn((
            timeout,
            WindowDraggedMarker {
                entity,
                display_id: active_display.id(),
            },
        ));
    }
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
pub(super) fn center_mouse_trigger(
    trigger: On<Add, FocusedMarker>,
    windows: Windows,
    config: Configuration,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    active_workspace: Query<&Scrolling, With<ActiveWorkspaceMarker>>,
) {
    let entity = trigger.event().entity;
    let Some(window) = windows.get(entity) else {
        return;
    };
    if active_workspace
        .iter()
        .next()
        .is_some_and(|scrolling| scrolling.is_user_swiping)
    {
        debug!("Suppressing center mouse due to a swipe");
        return;
    }

    if config.mouse_follows_focus()
        && !config.skip_reshuffle()
        && config.ffm_flag().is_none_or(|id| id != window.id())
        && let Some(frame) = windows.moving_frame(entity)
    {
        let display_bounds = active_display.bounds();
        let visible = display_bounds.intersect(frame);
        let origin = visible.center();
        debug!("centering on {} {origin}", window.id());
        window_manager.warp_mouse(origin);
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn dim_window_trigger(
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
pub(super) fn dim_remove_window_trigger(
    trigger: On<Remove, FocusedMarker>,
    windows: Windows,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    theme: Option<Res<SystemTheme>>,
) {
    let Some(window) = windows.get(trigger.event().entity) else {
        return;
    };

    let dark = theme.is_some_and(|theme| theme.is_dark);
    if let Some(dim_ratio) = config.window_dim_ratio(dark) {
        window_manager.dim_windows(&[window.id()], dim_ratio);
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
    mut config: Configuration,
    mouse_held: Query<&MouseHeldMarker>,
    mut commands: Commands,
) {
    const STRAY_FOCUS_RETRY_SEC: u64 = 2;

    let Event::WindowFocused { window_id } = trigger.event().0 else {
        return;
    };

    if let Some((window, _)) = windows.focused()
        && window.id() == window_id
    {
        return;
    }

    let Some((window, entity, parent)) = windows.find_parent(window_id) else {
        let timeout = Timeout::new(Duration::from_secs(STRAY_FOCUS_RETRY_SEC), None);
        commands.spawn((timeout, StrayFocusEvent(window_id)));
        return;
    };

    let Ok(app) = applications.get(parent) else {
        warn!("Unable to get parent for window {}.", window.id());
        return;
    };

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

    let focus = windows.focused().map(|(_, entity)| entity);
    for (window, entity) in windows.iter() {
        let Ok(mut cmd) = commands.get_entity(entity) else {
            continue;
        };
        let focused = focus.is_some_and(|focus| entity == focus);
        if focused && window.id() != window_id {
            cmd.try_remove::<FocusedMarker>();
        }
        if !focused && window.id() == window_id {
            cmd.try_insert(FocusedMarker);
        }
    }

    debug!("focused window id {}", window.id());

    update_passthrough(window, app, config.config());

    commands.entity(entity).try_insert(FocusedMarker);

    if !(config.skip_reshuffle() || config.initializing() || !mouse_held.is_empty()) {
        if config.auto_center()
            && let Some((_, _, None)) = windows.get_managed(entity)
            && let Some(size) = windows.size(entity)
            && let Some(mut origin) = windows.origin(entity)
        {
            let center = active_display.bounds().center();
            origin.x = center.x - size.x / 2;
            reposition_entity(entity, origin, &mut commands);
        }
        reshuffle_around(entity, &mut commands);
    }

    // Check if the reshuffle was caused by a keyboard switch or mouse move.
    // Skip reshuffle if caused by mouse - because then it won't center.
    if config.ffm_flag().is_none() {
        config.set_skip_reshuffle(false);
    }
    config.set_ffm_flag(None);
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
        Event::ApplicationLaunched { psn, observer } => {
            if find_process(psn).is_none() {
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
    mut active_display: ActiveDisplayMut,
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
    let display_bounds = active_display
        .display()
        .actual_display_bounds(active_display.dock(), config.config());
    let active_strip = active_display.active_strip();

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
        target_frame = clamp_origin_to_bounds(target_frame, target_frame.size(), display_bounds);
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

    if let Some(neighbour) = active_strip
        .left_neighbour(entity)
        .or_else(|| active_strip.right_neighbour(entity))
    {
        debug!("Reshuffling around its neighbour {neighbour}.");
        reshuffle_around(neighbour, &mut commands);
    }
    active_strip.remove(entity);
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn window_minimized_trigger(
    trigger: On<Add, Unmanaged>,
    windows: Windows,
    mut active_display: ActiveDisplayMut,
    mut config: Configuration,
    mut commands: Commands,
) {
    let entity = trigger.event().entity;
    if let Some((_, _, Some(Unmanaged::Minimized | Unmanaged::Hidden))) =
        windows.get_managed(entity)
    {
        debug!("Entity {entity} is minimized or hidden.");
        let display_bounds = active_display.bounds();
        let active_strip = active_display.active_strip();
        give_away_focus(
            entity,
            &windows,
            active_strip,
            &display_bounds,
            &mut config,
            &mut commands,
        );
        active_strip.remove(entity);
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn window_managed_trigger(
    trigger: On<Remove, Unmanaged>,
    mut active_display: ActiveDisplayMut,
    windows: Windows,
    apps: Query<(Entity, &Application)>,
    config: Configuration,
    mut commands: Commands,
) {
    let entity = trigger.event().entity;

    reinsert_window_into_layout(
        entity,
        &mut active_display,
        &windows,
        apps,
        config.config(),
        &mut commands,
    );
}

#[instrument(level = Level::DEBUG, skip_all, fields(entity))]
fn reinsert_window_into_layout(
    entity: Entity,
    active_display: &mut ActiveDisplayMut,
    windows: &Windows,
    apps: Query<(Entity, &Application)>,
    config: &Config,
    commands: &mut Commands,
) {
    debug!("Entity {entity} is managed again.");
    let display_bounds = active_display.bounds();
    let active_strip = active_display.active_strip();

    if let Some(window) = windows.get(entity)
        && let Some((_, app)) = windows
            .find_parent(window.id())
            .and_then(|(_, _, parent)| apps.get(parent).ok())
    {
        let properties = WindowProperties::new(app, window, config);

        if let Some(width_ratio) = properties.width_ratio() {
            let (_, pad_right, _, pad_left) = config.edge_padding();
            let padded_width = display_bounds.width() - pad_left - pad_right;
            let width = (f64::from(padded_width) * width_ratio).round() as i32;
            let height = window.frame().height();
            resize_entity(entity, Size::new(width, height), commands);
        }

        if properties.floating() {
            return;
        }
        if let Some(index) = properties.insertion() {
            active_strip.insert_at(index, entity);
            reshuffle_around(entity, commands);
            return;
        }
    }

    active_strip.append(entity);
    reshuffle_around(entity, commands);
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
        error!("Trying to destroy non-existing window {window_id}.");
        return;
    };

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
            config.config(),
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
    config: &Config,
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
        if let Some((rx, ry, rw, rh)) = properties.iter().find_map(WindowParams::grid_ratios) {
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
    if let Some(width) = properties.iter().find_map(|props| props.width) {
        let bounds = active_display.bounds();
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
        && let Some((focus, _)) = windows.focused()
        && let Some(psn) = windows.psn(focus.id(), &apps)
    {
        debug!(
            "Not focusing new window {entity}, keeping focus on '{}'",
            focus.title().unwrap_or_default()
        );
        focus.focus_with_raise(psn);
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

    if let Some(mut strip) = workspaces
        .iter_mut()
        .find(|strip| strip.index_of(entity).is_ok())
    {
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
pub(super) fn window_to_native_fullscreen(
    trigger: On<Add, NativeFullscreenMarker>,
    mut active_display: ActiveDisplayMut,
) {
    let entity = trigger.event().entity;
    let layout_strip = active_display.active_strip();
    layout_strip.remove(entity);
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn window_from_native_fullscreen(
    trigger: On<Remove, NativeFullscreenMarker>,
    mut active_display: ActiveDisplayMut,
    windows: Windows,
    apps: Query<(Entity, &Application)>,
    config: Configuration,
    mut commands: Commands,
) {
    let entity = trigger.event().entity;

    reinsert_window_into_layout(
        entity,
        &mut active_display,
        &windows,
        apps,
        config.config(),
        &mut commands,
    );
}

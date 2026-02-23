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
    ActiveDisplayMarker, BProcess, FocusedMarker, FreshMarker, Initializing, MissionControlActive,
    SpawnWindowTrigger, StrayFocusEvent, Timeout, Unmanaged, WMEventTrigger, WindowDraggedMarker,
};
use crate::config::{Config, WindowParams};
use crate::ecs::params::{ActiveDisplay, ActiveDisplayMut, Configuration, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, LocateDockTrigger, SendMessageTrigger, WindowSwipeMarker,
    reposition_entity, reshuffle_around, resize_entity,
};
use crate::errors::Result;
use crate::events::Event;
use crate::manager::{
    Application, Display, LayoutStrip, Origin, Process, Size, Window, WindowManager, WindowPadding,
    irect_from,
};
use crate::platform::{PlatformCallbacks, WinID, WorkspaceId};
use crate::util::symlink_target;

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
#[allow(clippy::needless_pass_by_value)]
pub(super) fn mouse_down_trigger(
    trigger: On<WMEventTrigger>,
    windows: Windows,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    mission_control_active: Res<MissionControlActive>,
    mut commands: Commands,
) {
    let Event::MouseDown { point } = trigger.event().0 else {
        return;
    };
    if mission_control_active.0 {
        return;
    }
    trace!("{point:?}");

    let Some((window, entity)) = window_manager
        .0
        .find_window_at_point(&point)
        .ok()
        .and_then(|window_id| windows.find(window_id))
    else {
        return;
    };

    if window.frame().min.x < 0
        || window.frame().min.x > active_display.bounds().width() - window.frame().width()
    {
        reshuffle_around(entity, &mut commands);
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

fn windows_not_in_strip<F: Fn(WinID) -> Option<Entity>>(
    workspace_id: WorkspaceId,
    find_window: F,
    strip: &LayoutStrip,
    window_manager: &WindowManager,
) -> Result<Vec<Entity>> {
    let moved_windows = window_manager
        .windows_in_workspace(workspace_id)
        .map(|ids| {
            ids.into_iter()
                .filter_map(find_window)
                // Filter out the ones already in this workspace.
                .filter(|entity| strip.index_of(*entity).is_err())
                .collect::<Vec<_>>()
        })?;
    Ok(moved_windows)
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn workspace_change_trigger(
    trigger: On<WMEventTrigger>,
    active_display: Single<&Display, With<ActiveDisplayMarker>>,
    workspaces: Query<(&LayoutStrip, Entity, Has<ActiveWorkspaceMarker>)>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let Event::SpaceChanged = trigger.event().0 else {
        return;
    };

    let Ok(workspace_id) = window_manager.active_display_space(active_display.id()) else {
        error!("Unable to get active workspace id!");
        return;
    };

    for (strip, entity, active) in workspaces {
        if active && strip.id() != workspace_id {
            debug!("Workspace id {} no longer active", strip.id());
            commands
                .entity(entity)
                .try_remove::<ActiveWorkspaceMarker>();
        }
        if !active && strip.id() == workspace_id {
            debug!("Workspace id {} is active", strip.id());
            commands.entity(entity).try_insert(ActiveWorkspaceMarker);
        }
    }
}

#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
pub(super) fn active_workspace_trigger(
    trigger: On<Add, ActiveWorkspaceMarker>,
    windows: Windows,
    mut workspaces: Query<&mut LayoutStrip, With<ChildOf>>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let Ok(active_strip) = workspaces.get(trigger.entity) else {
        return;
    };
    let workspace_id = active_strip.id();
    debug!("workspace {workspace_id}");

    let find_window = |window_id| windows.find_managed(window_id).map(|(_, entity)| entity);
    let Ok(moved_windows) =
        windows_not_in_strip(workspace_id, find_window, active_strip, &window_manager).inspect_err(
            |err| {
                warn!("unable to get windows in the current workspace: {err}");
            },
        )
    else {
        return;
    };

    for entity in moved_windows {
        debug!("Window {entity} moved to workspace {workspace_id}.");

        workspaces.iter_mut().for_each(|mut strip| {
            strip.remove(entity);
            if strip.id() == workspace_id {
                strip.append(entity);
            }
        });

        reshuffle_around(entity, &mut commands);
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
    windows: Windows,
    processes: Query<(&BProcess, &Children)>,
    applications: Query<&Application>,
    mut config: Configuration,
    mut commands: Commands,
) {
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
    let Some(app) = children
        .first()
        .and_then(|entity| applications.get(*entity).ok())
    else {
        error!("No application for process '{}'.", process.name());
        return;
    };
    debug!("front switching process: {}", process.name());

    if let Ok(focused_id) = app.focused_window_id().inspect_err(|err| {
        warn!("can not get current focus: {err}");
    }) {
        commands.trigger(WMEventTrigger(Event::WindowFocused {
            window_id: focused_id,
        }));
    } else if let Some((_, entity)) = windows.focused() {
        warn!("front_switched: removing FocusedMarker from {entity} with no replacement!");
        config.set_ffm_flag(None);
        commands.entity(entity).try_remove::<FocusedMarker>();
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn center_mouse_trigger(
    trigger: On<Add, FocusedMarker>,
    active_display: ActiveDisplay,
    windows: Windows,
    window_manager: Res<WindowManager>,
    config: Configuration,
) {
    let Some(window) = windows.get(trigger.event().entity) else {
        return;
    };

    if config.mouse_follows_focus()
        && !config.skip_reshuffle()
        && config.ffm_flag().is_none_or(|id| id != window.id())
    {
        debug!("centering on {}", window.id());
        window_manager.center_mouse(
            // If auto-centering, then just warp the mouse to the center of screen.
            (!config.auto_center()).then_some(window),
            &active_display.bounds(),
        );
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
pub(super) fn window_focused_trigger(
    trigger: On<WMEventTrigger>,
    applications: Query<&Application>,
    windows: Windows,
    active_display: ActiveDisplay,
    mut config: Configuration,
    initializing: Option<Res<Initializing>>,
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

    debug!("window id {}", window.id());

    let Ok(app) = applications.get(parent) else {
        warn!("Unable to get parent for window {}.", window.id());
        return;
    };
    if !app.is_frontmost() {
        return;
    }

    commands.entity(entity).try_insert(FocusedMarker);

    if !config.skip_reshuffle() && initializing.is_none() {
        if config.auto_center()
            && let Some((_, _, None)) = windows.get_managed(entity)
        {
            let center = active_display.bounds().center();
            let origin = IRect::from_center_size(center, window.frame().size()).min;
            reposition_entity(
                entity,
                active_display.display().absolute_coords(origin),
                active_display.id(),
                &mut commands,
            );
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

/// Handles swipe gesture events, potentially triggering window sliding.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the swipe event.
/// * `active_display` - A query for the active display.
/// * `focused_window` - A query for the focused window.
/// * `main_cid` - The main connection ID resource.
/// * `config` - The optional configuration resource.
/// * `commands` - Bevy commands to trigger events.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn swipe_gesture_trigger(
    trigger: On<WMEventTrigger>,
    focused_window: Single<(&Window, Entity), With<FocusedMarker>>,
    active_display: ActiveDisplay,
    config: Configuration,
    mut commands: Commands,
) {
    let Event::Swipe { ref deltas } = trigger.event().0 else {
        return;
    };
    if config
        .swipe_gesture_fingers()
        .is_none_or(|fingers| deltas.len() != fingers)
    {
        return;
    }
    let swipe_resolution = 1.0 / f64::from(active_display.bounds().width());
    let delta = deltas.iter().sum::<f64>();
    if delta.abs() < swipe_resolution {
        return;
    }

    let (_, entity) = *focused_window;
    commands.entity(entity).try_insert(WindowSwipeMarker(delta));
}

/// Handles Mission Control events, updating the `MissionControlActive` resource.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the Mission Control event.
/// * `mission_control_active` - The `MissionControlActive` resource.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn mission_control_trigger(
    trigger: On<WMEventTrigger>,
    mut mission_control_active: ResMut<MissionControlActive>,
) {
    match trigger.event().0 {
        Event::MissionControlShowAllWindows
        | Event::MissionControlShowFrontWindows
        | Event::MissionControlShowDesktop => {
            mission_control_active.as_mut().0 = true;
        }
        Event::MissionControlExit => {
            mission_control_active.as_mut().0 = false;
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
pub(super) fn window_unmanaged_trigger(
    trigger: On<Add, Unmanaged>,
    windows: Windows,
    mut active_display: ActiveDisplayMut,
    mut config: Configuration,
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
    let Some(marker) = windows
        .get_managed(trigger.event().entity)
        .and_then(|(_, _, marker)| marker)
    else {
        return;
    };
    let display_bounds = active_display.bounds();
    let display_id = active_display.id();
    let active_strip = active_display.active_strip();

    match marker {
        Unmanaged::Floating => {
            debug!("Entity {entity} is floating.");

            let Some(window) = windows.get(entity) else {
                return;
            };
            let frame = window.frame();
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
                    display_id,
                    &mut commands,
                );
            }
            if target_frame.min != frame.min {
                reposition_entity(entity, target_frame.min, display_id, &mut commands);
            }

            if let Some(neighbour) = active_strip
                .left_neighbour(entity)
                .or_else(|| active_strip.right_neighbour(entity))
            {
                debug!("Reshuffling around its neighbour {neighbour}.");
                reshuffle_around(neighbour, &mut commands);
            }
        }

        Unmanaged::Minimized | Unmanaged::Hidden => {
            debug!("Entity {entity} is minimized.");
            give_away_focus(entity, &windows, active_strip, &mut config, &mut commands);
        }
    }
    active_strip.remove(entity);
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn window_managed_trigger(
    trigger: On<Remove, Unmanaged>,
    mut active_display: ActiveDisplayMut,
    mut commands: Commands,
) {
    let entity = trigger.event().entity;
    debug!("Entity {entity} is managed again.");

    active_display.active_strip().append(entity);
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
    config: &mut Configuration,
    commands: &mut Commands,
) {
    // Move focus to a left neighbour if the panel has more windows.
    let other_window = if active_strip.len() > 1
        && let Some((window, neighbour)) = active_strip
            .left_neighbour(entity)
            .or_else(|| active_strip.right_neighbour(entity))
            .and_then(|e| windows.get(e).zip(Some(e)))
    {
        let window_id = window.id();
        debug!("giving away focus to neighbour {neighbour} {window_id}");
        Some((window_id, neighbour))
    } else {
        // Unmanaged window was despawned. Raise the first managed window in the workspace.
        active_strip
            .get(0)
            .ok()
            .and_then(|column| column.top())
            .and_then(|entity| windows.get(entity).zip(Some(entity)))
            .map(|(window, entity)| (window.id(), entity))
            .inspect(|(window_id, entity)| {
                debug!("giving away focus to first window {entity} {window_id}");
            })
    };

    if let Some((window_id, entity)) = other_window {
        config.set_ffm_flag(None);
        commands.trigger(WMEventTrigger(Event::WindowFocused { window_id }));
        reshuffle_around(entity, commands);
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
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_window_trigger(
    mut trigger: On<SpawnWindowTrigger>,
    windows: Windows,
    mut apps: Query<(Entity, &mut Application)>,
    mut active_display: ActiveDisplayMut,
    mut config: Configuration,
    initializing: Option<Res<Initializing>>,
    mut commands: Commands,
) {
    let initializing = initializing.is_some();
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
        let bundle_id = app.bundle_id().unwrap_or_default();
        debug!(
            "window {} isroot {} bundle_id {}",
            window_id,
            window.is_root(),
            bundle_id,
        );

        let title = window.title().unwrap_or_default();
        let properties = config.find_window_properties(&title, bundle_id);
        if !properties.is_empty() {
            debug!("Applying window properties for '{}'", window.id());
        }

        apply_window_defaults(
            &mut window,
            &mut active_display,
            &properties,
            config.edge_padding(),
        );

        // Insert the window into the internal Bevy state.
        let entity = commands.spawn((window, ChildOf(app_entity))).id();

        apply_window_properties(
            entity,
            &properties,
            &mut active_display,
            &windows,
            &mut apps,
            &mut config,
            initializing,
            &mut commands,
        );
    }
}

#[allow(clippy::cast_possible_truncation)]
fn apply_window_defaults(
    window: &mut Window,
    active_display: &mut ActiveDisplayMut,
    properties: &[WindowParams],
    edge_padding: (i32, i32, i32, i32),
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
            let x = bounds.width() * rx;
            let y = bounds.height() * ry;
            let w = bounds.width() * rw;
            let h = bounds.height() * rh;
            window.reposition(Origin::new(x, y));
            window.resize(Size::new(w, h), bounds.width());
        }
        return;
    }

    _ = window
        .update_frame(&active_display.bounds())
        .inspect_err(|err| error!("{err}"));

    // Apply configured width AFTER update_frame so it isn't overwritten.
    // Use padded display width (matching window_resize command behavior).
    if let Some(width) = properties.iter().find_map(|props| props.width) {
        let bounds = active_display.bounds();
        let (_, pad_right, _, pad_left) = edge_padding;
        let padded_width = bounds.width() - pad_left - pad_right;
        let new_width = (f64::from(padded_width) * width).round() as i32;
        let height = window.frame().height();
        window.resize(Size::new(new_width, height), bounds.width());
        // Re-read the actual OS size: the app may enforce a minimum width
        // that differs from our request.
        _ = window
            .update_frame(&bounds)
            .inspect_err(|err| error!("{err}"));
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_window_properties(
    entity: Entity,
    properties: &[WindowParams],
    active_display: &mut ActiveDisplayMut,
    windows: &Windows,
    apps: &mut Query<(Entity, &mut Application)>,
    config: &mut Configuration,
    initializing: bool,
    commands: &mut Commands,
) {
    let floating = properties
        .iter()
        .find_map(|props| props.floating)
        .unwrap_or(false);
    let wanted_insertion = properties.iter().find_map(|props| props.index);
    let dont_focus = properties
        .iter()
        .find_map(|props| props.dont_focus)
        .unwrap_or(false);

    if floating {
        // Avoid managing window if it's floating.
        commands.entity(entity).try_insert(Unmanaged::Floating);
        return;
    }

    let strip = active_display.active_strip();

    // Attempt inserting the window at a pre-defined position.
    let insert_at = wanted_insertion.map_or_else(
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

    if initializing {
        // During init, skip per-window reshuffles. finish_setup does a single
        // reshuffle after all windows are added.
    } else if dont_focus {
        let mut lens = apps.transmute_lens::<&Application>();
        if let Some((focus, _)) = windows.focused()
            && let Some(psn) = windows.psn(focus.id(), &lens.query())
        {
            debug!(
                "Not focusing new window {entity}, keeping focus on '{}'",
                focus.title().unwrap_or_default()
            );
            focus.focus_with_raise(psn);
            config.set_skip_reshuffle(true);
        }
    } else {
        reshuffle_around(entity, commands);
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn refresh_configuration_trigger(
    trigger: On<WMEventTrigger>,
    window_manager: Res<WindowManager>,
    mut config: ResMut<Config>,
    watcher: Option<NonSendMut<Box<dyn Watcher>>>,
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
        if let Ok(mut entity_cmmands) = commands.get_entity(entity) {
            entity_cmmands.try_insert(dock);
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

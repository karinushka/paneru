use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::{ChildOf, Children};
use bevy::ecs::lifecycle::Add;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Has, With};
use bevy::ecs::system::{Commands, Query, Res, ResMut, Single};
use log::{debug, error, trace, warn};
use objc2_core_foundation::{CGPoint, CGRect};
use std::mem::take;
use std::time::Duration;
use stdext::function_name;

use crate::app::Application;
use crate::config::WindowParams;
use crate::errors::{Error, Result};
use crate::events::{
    ActiveDisplayMarker, BProcess, Event, FocusedMarker, FreshMarker, MissionControlActive,
    OrphanedPane, RepositionMarker, ReshuffleAroundTrigger, SpawnWindowTrigger, StrayFocusEvent,
    Timeout, WMEventTrigger, WindowDraggedMarker,
};
use crate::manager::WindowManager;
use crate::params::Configuration;
use crate::process::Process;
use crate::skylight::WinID;
use crate::windows::{Display, Panel, Window, WindowPane, ax_window_pid};

const WINDOW_HIDDEN_THRESHOLD: f64 = 10.0;

/// Registers all the event triggers for the window manager.
pub fn register_triggers(app: &mut bevy::app::App) {
    app.add_observer(mouse_moved_trigger)
        .add_observer(mouse_down_trigger)
        .add_observer(mouse_dragged_trigger)
        .add_observer(workspace_change_trigger)
        .add_observer(display_change_trigger)
        .add_observer(active_display_trigger)
        .add_observer(display_add_trigger)
        .add_observer(display_remove_trigger)
        .add_observer(display_moved_trigger)
        .add_observer(front_switched_trigger)
        .add_observer(window_focused_trigger)
        .add_observer(reshuffle_around_trigger)
        .add_observer(swipe_gesture_trigger)
        .add_observer(mission_control_trigger)
        .add_observer(application_event_trigger)
        .add_observer(dispatch_application_messages)
        .add_observer(window_resized_trigger)
        .add_observer(window_destroyed_trigger)
        .add_observer(spawn_window_trigger);
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
fn mouse_moved_trigger(
    trigger: On<WMEventTrigger>,
    windows: Query<&Window>,
    focused_window: Single<&Window, With<FocusedMarker>>,
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
        trace!("{}: ffm_window_id > 0", function_name!());
        return;
    }
    let Ok(window_id) = window_manager.find_window_at_point(&point) else {
        debug!(
            "{}: can not find window at point {point:?}",
            function_name!()
        );
        return;
    };
    if focused_window.id() == window_id {
        trace!("{}: allready focused {}", function_name!(), window_id);
        return;
    }
    let Some(window) = windows.iter().find(|window| window.id() == window_id) else {
        trace!(
            "{}: can not find focused window: {}",
            function_name!(),
            window_id
        );
        return;
    };
    if !window.is_eligible() {
        trace!("{}: {} not eligible", function_name!(), window_id);
        return;
    }

    let child_windows = window_manager
        .get_associated_windows(window_id)
        .iter()
        .filter_map(|child_wid| {
            let child_window = windows.iter().find(|window| window.id() == *child_wid);
            if child_window.is_none() {
                warn!(
                    "{}: Unable to find child window {child_wid}.",
                    function_name!()
                );
            }
            child_window
        })
        .collect::<Vec<_>>();

    let valid_role = child_windows.into_iter().find(|window| {
        window
            .valid_role()
            .inspect_err(|err| {
                warn!(
                    "{}: Can not get role for children of {window_id}: {err}",
                    function_name!(),
                );
            })
            .is_ok()
    });
    let window = valid_role.unwrap_or(window);

    // Do not reshuffle windows due to moved mouse focus.
    config.set_skip_reshuffle(true);
    config.set_ffm_flag(Some(window.id()));
    window.focus_without_raise(&focused_window);
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
fn mouse_down_trigger(
    trigger: On<WMEventTrigger>,
    windows: Query<&Window>,
    active_display: Single<&Display, With<ActiveDisplayMarker>>,
    window_manager: Res<WindowManager>,
    mission_control_active: Res<MissionControlActive>,
    mut commands: Commands,
) {
    let Event::MouseDown { point } = trigger.event().0 else {
        return;
    };
    trace!("{}: {point:?}", function_name!());
    if mission_control_active.0 {
        return;
    }

    let Some(window) = window_manager
        .find_window_at_point(&point)
        .ok()
        .and_then(|window_id| windows.iter().find(|window| window.id() == window_id))
    else {
        return;
    };
    if !window.fully_visible(&active_display.bounds) {
        commands.trigger(ReshuffleAroundTrigger(window.id()));
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
fn mouse_dragged_trigger(
    trigger: On<WMEventTrigger>,
    windows: Query<(Entity, &Window)>,
    mut drag_marker: Query<(&mut Timeout, &mut WindowDraggedMarker)>,
    window_manager: Res<WindowManager>,
    mission_control_active: Res<MissionControlActive>,
    mut commands: Commands,
) {
    const DRAG_MARKER_TIMEOUT_MS: u64 = 3000;
    let Event::MouseDragged { point } = trigger.event().0 else {
        return;
    };
    if mission_control_active.0 {
        #[warn(clippy::needless_return)]
        return;
    }

    let Some((entity, window)) = window_manager
        .find_window_at_point(&point)
        .ok()
        .and_then(|window_id| windows.iter().find(|(_, window)| window.id() == window_id))
    else {
        return;
    };

    if let Ok((mut timeout, mut marker)) = drag_marker.single_mut() {
        // Change the current marker contents and refresh the timer.
        if entity != marker.0 {
            marker.as_mut().0 = entity;
            timeout.timer.reset();
        }
    } else {
        trace!(
            "{}: Adding a drag marker ({entity}) to window id {}.",
            function_name!(),
            window.id(),
        );
        let timeout = Timeout::new(Duration::from_millis(DRAG_MARKER_TIMEOUT_MS), None);
        commands.spawn((timeout, WindowDraggedMarker(entity)));
    }
}

#[allow(clippy::needless_pass_by_value)]
fn workspace_change_trigger(
    trigger: On<WMEventTrigger>,
    mut active_display: Single<&mut Display, With<ActiveDisplayMarker>>,
    focused_window: Single<(&Window, Entity), With<FocusedMarker>>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let Event::SpaceChanged = trigger.event().0 else {
        return;
    };

    let Ok(workspace_id) = window_manager.active_display_space(active_display.id()) else {
        return;
    };
    let Ok(panel) = active_display.active_panel(workspace_id) else {
        return;
    };
    let (window, entity) = *focused_window;
    if !window.managed() || panel.index_of(entity).is_ok() {
        // Window is either unmanaged or already in the current space.
        return;
    }

    let windows = window_manager.windows_in_workspace(workspace_id);
    if !windows.is_ok_and(|windows| {
        windows
            .into_iter()
            .any(|window_id| window.id() == window_id)
    }) {
        // The focused window is not present in the current workspace, don't move it.
        return;
    }
    debug!(
        "{}: Window {} moved to workspace {workspace_id}.",
        function_name!(),
        window.id(),
    );

    active_display.remove_window(entity);
    if let Ok(panel) = active_display.active_panel(workspace_id) {
        panel.append(entity);
    }

    commands.trigger(ReshuffleAroundTrigger(window.id()));
}

/// Handles display change events.
///
/// When the active display or space changes, this function ensures that the window manager's
/// internal state is updated. It marks the new active display with `FocusedMarker` and moves
/// the focused window to the correct `WindowPane` if it has been moved to a different display
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
fn display_change_trigger(
    trigger: On<WMEventTrigger>,
    displays: Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let Event::DisplayChanged = trigger.event().0 else {
        return;
    };

    let Ok(active_id) = window_manager.active_display_id() else {
        error!("{}: Unable to get active display id!", function_name!());
        return;
    };

    for (display, entity, focused) in displays {
        if focused && display.id() != active_id {
            debug!(
                "{}: Display id {} no longer active",
                function_name!(),
                display.id()
            );
            commands.entity(entity).remove::<ActiveDisplayMarker>();
        }
        if !focused && display.id() == active_id {
            debug!(
                "{}: Display id {} is active",
                function_name!(),
                display.id()
            );
            commands.entity(entity).insert(ActiveDisplayMarker);
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn active_display_trigger(
    trigger: On<Add, ActiveDisplayMarker>,
    mut displays: Query<&mut Display>,
    drag_marker: Single<(Entity, &WindowDraggedMarker)>,
    windows: Query<&Window>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let Ok(mut active_display) = displays.get_mut(trigger.event().entity) else {
        error!("{}: Can not find active display.", function_name!());
        return;
    };
    let active_display_id = active_display.id();
    let Ok(workspace_id) = window_manager.active_display_space(active_display_id) else {
        return;
    };
    let Ok(panel) = active_display.active_panel(workspace_id) else {
        return;
    };
    debug!(
        "{}: Display ({active_display_id}) changed, panel {panel}.",
        function_name!()
    );

    // This function will not run unless a WindowDraggedMarker exists.
    let WindowDraggedMarker(entity) = *drag_marker.1;
    let Ok(window) = windows.get(entity) else {
        return;
    };

    if !window.managed() || panel.index_of(entity).is_ok() {
        // Window is either unmanaged or already in the current space.
        return;
    }
    let windows = window_manager.windows_in_workspace(workspace_id);
    if !windows.is_ok_and(|windows| {
        windows
            .into_iter()
            .any(|window_id| window.id() == window_id)
    }) {
        // The focused window is not present in the current workspace, don't move it.
        return;
    }
    debug!(
        "{}: Window {} dragged to display {active_display_id}.",
        function_name!(),
        window.id(),
    );

    // Insert the window into the current panel.
    if let Ok(panel) = active_display.active_panel(workspace_id) {
        panel.append(entity);
    }
    // And then remove from all the other.
    displays.iter_mut().for_each(|mut display| {
        if display.id() != active_display_id {
            display.remove_window(entity);
        }
    });

    commands.trigger(ReshuffleAroundTrigger(window.id()));
}

/// Handles display added events.
/// It updates the list of displays and re-evaluates orphaned spaces.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the display event.
/// * `windows` - A query for all windows.
/// * `main_cid` - The main connection ID resource.
/// * `orphaned_spaces` - The resource for orphaned spaces.
/// * `commands` - Bevy commands to spawn/despawn entities and trigger events.
#[allow(clippy::needless_pass_by_value)]
fn display_add_trigger(
    trigger: On<WMEventTrigger>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let Event::DisplayAdded { display_id } = trigger.event().0 else {
        return;
    };

    debug!("{}: Display Added: {display_id:?}", function_name!());
    let Some(display) = window_manager
        .present_displays()
        .into_iter()
        .find(|display| display.id() == display_id)
    else {
        error!("{}: Unable to find added display!", function_name!());
        return;
    };

    for (id, pane) in &display.spaces {
        debug!("{}: Space {id} - {pane}", function_name!());
    }
    commands.spawn(display);
    commands.trigger(WMEventTrigger(Event::DisplayChanged));
}

/// Handles display removed events.
/// It identifies orphaned spaces from the removed display and moves them to other displays.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the display event.
/// * `displays` - A query for all displays.
/// * `windows` - A query for all windows.
/// * `orphaned_spaces` - The resource for orphaned spaces.
/// * `commands` - Bevy commands to despawn entities and trigger events.
#[allow(clippy::needless_pass_by_value)]
fn display_remove_trigger(
    trigger: On<WMEventTrigger>,
    mut displays: Query<(&mut Display, Entity)>,
    mut commands: Commands,
) {
    let Event::DisplayRemoved { display_id } = trigger.event().0 else {
        return;
    };
    debug!("{}: Display Removed: {display_id:?}", function_name!());
    let Some((mut display, entity)) = displays
        .iter_mut()
        .find(|(display, _)| display.id() == display_id)
    else {
        error!("{}: Unable to find removed display!", function_name!());
        return;
    };

    for (id, pane) in take(&mut display.spaces)
        .into_iter()
        .filter(|(_, pane)| pane.len() > 0)
    {
        debug!("{}: adding {pane} to orphaned list.", function_name!(),);
        commands.spawn(OrphanedPane { id, pane });
    }

    commands.entity(entity).despawn();
    commands.trigger(WMEventTrigger(Event::DisplayChanged));
}

/// Handles display moved events.
/// It updates the display's information and re-evaluates orphaned spaces.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the display event.
/// * `displays` - A query for all displays.
/// * `windows` - A query for all windows.
/// * `main_cid` - The main connection ID resource.
/// * `orphaned_spaces` - The resource for orphaned spaces.
/// * `commands` - Bevy commands to trigger events.
#[allow(clippy::needless_pass_by_value)]
fn display_moved_trigger(
    trigger: On<WMEventTrigger>,
    mut displays: Query<(&mut Display, Entity)>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let Event::DisplayMoved { display_id } = trigger.event().0 else {
        return;
    };

    debug!("{}: Display Moved: {display_id:?}", function_name!());
    let Some((mut display, _)) = displays
        .iter_mut()
        .find(|(display, _)| display.id() == display_id)
    else {
        error!("{}: Unable to find moved display!", function_name!());
        return;
    };
    let Some(moved_display) = window_manager
        .present_displays()
        .into_iter()
        .find(|display| display.id() == display_id)
    else {
        return;
    };
    *display = moved_display;

    for (id, pane) in &display.spaces {
        debug!("{}: Space {id} - {pane}", function_name!());
    }
    commands.trigger(WMEventTrigger(Event::DisplayChanged));
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
fn front_switched_trigger(
    trigger: On<WMEventTrigger>,
    processes: Query<(&BProcess, &Children)>,
    applications: Query<&Application>,
    mut commands: Commands,
) {
    let Event::ApplicationFrontSwitched { ref psn } = trigger.event().0 else {
        return;
    };
    let Some((BProcess(process), children)) =
        processes.iter().find(|process| &process.0.0.psn == psn)
    else {
        error!(
            "{}: Unable to find process with PSN {psn:?}",
            function_name!()
        );
        return;
    };

    if children.len() > 1 {
        warn!(
            "{}: Multiple apps registered to process '{}'.",
            function_name!(),
            process.name
        );
    }
    let Some(app) = children
        .first()
        .and_then(|entity| applications.get(*entity).ok())
    else {
        error!(
            "{}: No application for process '{}'.",
            function_name!(),
            process.name
        );
        return;
    };
    debug!("{}: {}", function_name!(), process.name);

    if let Ok(focused_id) = app.focused_window_id().inspect_err(|err| {
        warn!(
            "{}: keeping current focused window: {err}",
            function_name!()
        );
    }) {
        commands.trigger(WMEventTrigger(Event::WindowFocused {
            window_id: focused_id,
        }));
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
fn window_focused_trigger(
    trigger: On<WMEventTrigger>,
    applications: Query<&Application>,
    windows: Query<(&Window, Entity, &ChildOf, Has<FocusedMarker>)>,
    active_display: Single<&Display, With<ActiveDisplayMarker>>,
    window_manager: Res<WindowManager>,
    mut config: Configuration,
    mut commands: Commands,
) {
    const STRAY_FOCUS_RETRY_SEC: u64 = 2;

    let Event::WindowFocused { window_id } = trigger.event().0 else {
        return;
    };

    let Some((window, entity, child, _)) = windows
        .iter()
        .find(|(window, _, _, _)| window.id() == window_id)
    else {
        let timeout = Timeout::new(Duration::from_secs(STRAY_FOCUS_RETRY_SEC), None);
        commands.spawn((timeout, StrayFocusEvent(window_id)));
        return;
    };

    let mut previous_focus_id = None;
    for (window, entity, _, focused) in windows {
        if focused && window.id() != window_id {
            previous_focus_id = Some(window.id());
            commands.entity(entity).remove::<FocusedMarker>();
        }
        if !focused && window.id() == window_id {
            commands.entity(entity).insert(FocusedMarker);
        }
    }

    debug!("{}: window id {}", function_name!(), window.id());
    let Ok(app) = applications.get(child.parent()) else {
        warn!(
            "{}: Unable to get parent for window {}.",
            function_name!(),
            window.id()
        );
        return;
    };
    if !app.is_frontmost() {
        return;
    }

    if config.mouse_follows_focus()
        && previous_focus_id.is_none_or(|previous_id| previous_id != window_id)
        && config.ffm_flag().is_none_or(|id| id != window_id)
    {
        window_manager.center_mouse(window, &active_display.bounds);
    }

    commands.entity(entity).insert(FocusedMarker);
    config.set_ffm_flag(None);

    if config.skip_reshuffle() {
        config.set_skip_reshuffle(false);
    } else {
        commands.trigger(ReshuffleAroundTrigger(window.id()));
    }
}

/// A Bevy system that triggers the `reshuffle_around` logic in response to a `ReshuffleAroundTrigger` event.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the ID of the window to reshuffle around.
/// * `main_cid` - The main connection ID resource.
/// * `active_display` - A query for the active display.
/// * `windows` - A query for all windows.
/// * `commands` - Bevy commands to trigger events.
#[allow(clippy::needless_pass_by_value)]
fn reshuffle_around_trigger(
    trigger: On<ReshuffleAroundTrigger>,
    window_manager: Res<WindowManager>,
    mut active_display: Single<&mut Display, With<ActiveDisplayMarker>>,
    mut windows: Query<(&mut Window, Entity, Option<&RepositionMarker>)>,
    mut commands: Commands,
) {
    let Some((window, entity, _)) = windows
        .iter()
        .find(|(window, _, _)| window.id() == trigger.event().0)
    else {
        return;
    };
    if window.managed() {
        _ = reshuffle_around(
            &window_manager,
            &mut active_display,
            entity,
            &mut windows,
            &mut commands,
        )
        .inspect_err(|err| {
            error!("{}: failed with: {err}", function_name!());
        });
    }
}

/// Reshuffles windows around a given window entity within the active panel to ensure visibility.
/// Windows to the right and left of the focused window are repositioned.
///
/// # Arguments
///
/// * `main_cid` - The main connection ID.
/// * `active_display` - A query for the active display.
/// * `entity` - The `Entity` of the window to reshuffle around.
/// * `windows` - A query for all windows.
/// * `commands` - Bevy commands to trigger events.
#[allow(clippy::needless_pass_by_value)]
fn reshuffle_around(
    window_manager: &WindowManager,
    active_display: &mut Display,
    entity: Entity,
    windows: &mut Query<(&mut Window, Entity, Option<&RepositionMarker>)>,
    commands: &mut Commands,
) -> Result<()> {
    let display_bounds = active_display.bounds;
    let menubar_height = active_display.menubar_height;
    let active_panel = window_manager
        .active_display_space(active_display.id())
        .and_then(|active_space| active_display.active_panel(active_space))?;

    let (window, _, moving) = windows.get_mut(entity)?;
    let frame = window.expose_window(&display_bounds, moving, entity, commands);
    trace!(
        "{}: Moving window {} to {:?}",
        function_name!(),
        window.id(),
        frame.origin
    );
    let panel = active_panel
        .index_of(entity)
        .and_then(|index| active_panel.get(index))?;

    reposition_stack(
        frame.origin.x,
        &panel,
        frame.size.width,
        &display_bounds,
        menubar_height,
        windows,
        commands,
    );

    // Shuffling windows to the right of the focus.
    let mut upper_left = frame.origin.x + frame.size.width;
    _ = active_panel.access_right_of(entity, |panel| {
        let frame = panel
            .top()
            .and_then(|entity| windows.get(entity).ok())
            .map(|(window, _, _)| (window.id(), window.frame));
        if let Some((window_id, frame)) = frame {
            trace!(
                "{}: window {window_id} right: frame: {frame:?}",
                function_name!()
            );

            // Check for window getting off screen.
            if upper_left > display_bounds.size.width - WINDOW_HIDDEN_THRESHOLD {
                upper_left = display_bounds.size.width - WINDOW_HIDDEN_THRESHOLD;
            }

            if (frame.origin.x - upper_left).abs() > 0.1 {
                reposition_stack(
                    upper_left,
                    panel,
                    frame.size.width,
                    &display_bounds,
                    menubar_height,
                    windows,
                    commands,
                );
            }
            upper_left += frame.size.width;
        }
        true // continue through all windows
    });

    // Shuffling windows to the left of the focus.
    let mut upper_left = frame.origin.x;
    _ = active_panel.access_left_of(entity, |panel| {
        let frame = panel
            .top()
            .and_then(|entity| windows.get(entity).ok())
            .map(|(window, _, _)| (window.id(), window.frame));
        if let Some((window_id, frame)) = frame {
            trace!(
                "{}: window {window_id} left: frame: {frame:?}",
                function_name!()
            );

            // Check for window getting off screen.
            if upper_left < WINDOW_HIDDEN_THRESHOLD {
                upper_left = WINDOW_HIDDEN_THRESHOLD;
            }
            upper_left -= frame.size.width;

            if (frame.origin.x - upper_left).abs() > 0.1 {
                reposition_stack(
                    upper_left,
                    panel,
                    frame.size.width,
                    &display_bounds,
                    menubar_height,
                    windows,
                    commands,
                );
            }
        }
        true // continue through all windows
    });
    Ok(())
}

/// Repositions all windows within a given panel stack.
///
/// # Arguments
///
/// * `upper_left` - The x-coordinate of the upper-left corner of the stack.
/// * `panel` - The panel containing the windows to reposition.
/// * `width` - The width of each window in the stack.
/// * `display_bounds` - The bounds of the display.
/// * `menubar_height` - The height of the menu bar.
/// * `windows` - A query for all windows.
/// * `commands` - Bevy commands to trigger events.
fn reposition_stack(
    upper_left: f64,
    panel: &Panel,
    width: f64,
    display_bounds: &CGRect,
    menubar_height: f64,
    windows: &mut Query<(&mut Window, Entity, Option<&RepositionMarker>)>,
    commands: &mut Commands,
) {
    const REMAINING_THERSHOLD: f64 = 200.0;
    let display_height = display_bounds.size.height - menubar_height;
    let entities = match panel {
        Panel::Single(entity) => vec![*entity],
        Panel::Stack(stack) => stack.clone(),
    };
    let count: f64 = u32::try_from(entities.len()).unwrap().into();
    let mut fits = 0f64;
    let mut height = menubar_height;
    let mut remaining = display_height;
    for entity in &entities[0..entities.len() - 1] {
        remaining = display_height - height;
        if let Ok((window, _, _)) = windows.get(*entity) {
            if window.frame().size.height > remaining - REMAINING_THERSHOLD {
                trace!(
                    "{}: height {height}, remaining {remaining}",
                    function_name!()
                );
                break;
            }
            height += window.frame().size.height;
            fits += 1.0;
        }
    }
    let avg_height = remaining / (count - fits);
    trace!(
        "{}: fits {fits:.0} avg_height {avg_height:.0}",
        function_name!()
    );

    let mut y_pos = 0f64;
    for entity in entities {
        if let Ok((mut window, entity, _)) = windows.get_mut(entity) {
            let window_height = window.frame().size.height;

            commands.entity(entity).insert(RepositionMarker {
                origin: CGPoint {
                    x: upper_left,
                    y: y_pos,
                },
            });
            if fits > 0.0 {
                y_pos += window_height;
                fits -= 1.0;
            } else {
                window.resize(width, avg_height, display_bounds);
                y_pos += avg_height;
            }
        }
    }
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
fn swipe_gesture_trigger(
    trigger: On<WMEventTrigger>,
    active_display: Single<&Display, With<ActiveDisplayMarker>>,
    mut focused_window: Single<&mut Window, With<FocusedMarker>>,
    window_manager: Res<WindowManager>,
    config: Configuration,
    mut commands: Commands,
) {
    const SWIPE_THRESHOLD: f64 = 0.01;
    let Event::Swipe { ref deltas } = trigger.event().0 else {
        return;
    };
    if config
        .swipe_gesture_fingers()
        .is_some_and(|fingers| deltas.len() == fingers)
    {
        let delta = deltas.iter().sum::<f64>();
        if delta.abs() > SWIPE_THRESHOLD {
            slide_window(
                &window_manager,
                &mut focused_window,
                &active_display,
                delta,
                &mut commands,
            );
        }
    }
}

/// Slides a window horizontally based on a swipe gesture.
///
/// # Arguments
///
/// * `main_cid` - The main connection ID.
/// * `focused_window` - A query for the currently focused window.
/// * `active_display` - A reference to the active display.
/// * `delta_x` - The horizontal delta of the swipe gesture.
/// * `commands` - Bevy commands to trigger a reshuffle.
fn slide_window(
    window_manager: &WindowManager,
    window: &mut Window,
    active_display: &Display,
    delta_x: f64,
    commands: &mut Commands,
) {
    trace!("{}: Windows slide {delta_x}.", function_name!());
    let frame = window.frame();
    // Delta is relative to the touchpad size, so to avoid too fast movement we
    // scale it down by half.
    let x = frame.origin.x - (active_display.bounds.size.width / 2.0 * delta_x);
    window.reposition(
        x.min(active_display.bounds.size.width - frame.size.width)
            .max(0.0),
        frame.origin.y,
        &active_display.bounds,
    );
    window_manager.center_mouse(window, &active_display.bounds);
    commands.trigger(ReshuffleAroundTrigger(window.id()));
}

/// Handles Mission Control events, updating the `MissionControlActive` resource.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the Mission Control event.
/// * `mission_control_active` - The `MissionControlActive` resource.
#[allow(clippy::needless_pass_by_value)]
fn mission_control_trigger(
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
fn application_event_trigger(
    trigger: On<WMEventTrigger>,
    processes: Query<(&BProcess, Entity)>,
    mut commands: Commands,
) {
    const PROCESS_READY_TIMEOUT_SEC: u64 = 5;
    let find_process = |psn| {
        processes
            .iter()
            .find(|(BProcess(process), _)| &process.psn == psn)
    };

    match &trigger.event().0 {
        Event::ApplicationLaunched { psn, observer } => {
            if find_process(psn).is_none() {
                let process = Process::new(psn, observer.clone());
                let timeout = Timeout::new(
                    Duration::from_secs(PROCESS_READY_TIMEOUT_SEC),
                    Some(format!(
                        "{}: Process '{}' did not become ready in {PROCESS_READY_TIMEOUT_SEC}s.",
                        function_name!(),
                        process.name
                    )),
                );
                commands.spawn((FreshMarker, timeout, BProcess(process)));
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
fn dispatch_application_messages(
    trigger: On<WMEventTrigger>,
    mut windows: Query<(&mut Window, Entity)>,
    mut active_display: Single<&mut Display, With<ActiveDisplayMarker>>,
    applications: Query<(&Application, &Children)>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    match &trigger.event().0 {
        Event::WindowCreated { element } => match Window::new(element) {
            Ok(window) => {
                commands.trigger(SpawnWindowTrigger(vec![window]));
            }
            Err(err) => debug!("{}: not adding window {element:?}: {err}", function_name!(),),
        },

        Event::WindowMinimized { window_id } => {
            _ = window_minimized(
                *window_id,
                &mut windows,
                &mut active_display,
                &window_manager,
                &mut commands,
            )
            .inspect_err(|err| warn!("{}: Minimizing window: {err}", function_name!()));
        }

        Event::WindowDeminimized { window_id } => {
            _ = window_unminimized(
                *window_id,
                &mut windows,
                &mut active_display,
                &window_manager,
                &mut commands,
            )
            .inspect_err(|err| warn!("{}: Unminimizing window: {err}", function_name!()));
        }

        Event::ApplicationHidden { pid } => {
            let Some((_, children)) = applications.iter().find(|(app, _)| app.pid() == *pid) else {
                warn!("{}: Unable to find with pid {pid}", function_name!());
                return;
            };

            let window_ids = children
                .iter()
                .filter_map(|entity| windows.get(*entity).map(|(window, _)| window.id()).ok())
                .collect::<Vec<_>>();
            for window_id in window_ids {
                _ = window_minimized(
                    window_id,
                    &mut windows,
                    &mut active_display,
                    &window_manager,
                    &mut commands,
                )
                .inspect_err(|err| warn!("{}: Minimizing window: {err}", function_name!()));
            }
        }

        Event::ApplicationVisible { pid } => {
            let Some((_, children)) = applications.iter().find(|(app, _)| app.pid() == *pid) else {
                warn!(
                    "{}: Unable to find application with pid {pid}",
                    function_name!()
                );
                return;
            };

            let window_ids = children
                .iter()
                .filter_map(|entity| windows.get(*entity).map(|(window, _)| window.id()).ok())
                .collect::<Vec<_>>();
            for window_id in window_ids {
                _ = window_unminimized(
                    window_id,
                    &mut windows,
                    &mut active_display,
                    &window_manager,
                    &mut commands,
                )
                .inspect_err(|err| warn!("{}: Unminimizing window: {err}", function_name!()));
            }
        }

        _ => (),
    }
}

fn window_minimized(
    window_id: WinID,
    windows: &mut Query<(&mut Window, Entity)>,
    active_display: &mut Display,
    window_manager: &WindowManager,
    commands: &mut Commands,
) -> Result<()> {
    let (mut window, entity) = windows
        .iter_mut()
        .find(|(window, _)| window.id() == window_id)
        .ok_or(Error::InvalidWindow)?;

    window.manage(false);

    let mut lens = windows.transmute_lens::<&Window>();
    let active_panel = window_manager
        .active_display_space(active_display.id())
        .and_then(|active_space| active_display.active_panel(active_space))?;
    give_away_focus(entity, &lens.query(), active_panel, commands);

    active_display.remove_window(entity);
    Ok(())
}

fn window_unminimized(
    window_id: WinID,
    windows: &mut Query<(&mut Window, Entity)>,
    active_display: &mut Display,
    window_manager: &WindowManager,
    commands: &mut Commands,
) -> Result<()> {
    let active_panel = window_manager
        .active_display_space(active_display.id())
        .and_then(|active_space| active_display.active_panel(active_space))?;
    let (mut window, entity) = windows
        .iter_mut()
        .find(|(window, _)| window.id() == window_id)
        .ok_or(Error::InvalidWindow)?;

    window.manage(true);
    active_panel.append(entity);
    commands.trigger(ReshuffleAroundTrigger(window_id));
    Ok(())
}

/// Handles the event when a window is resized. It updates the window's frame and reshuffles windows.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the window resized event.
/// * `windows` - A mutable query for all `Window` components.
/// * `displays` - A query for the active display.
/// * `commands` - Bevy commands to trigger events.
#[allow(clippy::needless_pass_by_value)]
fn window_resized_trigger(
    trigger: On<WMEventTrigger>,
    mut windows: Query<(&mut Window, Entity)>,
    active_display: Single<&mut Display, With<ActiveDisplayMarker>>,
    mut commands: Commands,
) {
    let Event::WindowResized { window_id } = trigger.event().0 else {
        return;
    };
    let Some((mut window, _)) = windows
        .iter_mut()
        .find(|(window, _)| window.id() == window_id)
    else {
        return;
    };
    _ = window.update_frame(Some(&active_display.bounds));
    commands.trigger(ReshuffleAroundTrigger(window.id()));
}

/// Handles the event when a window is destroyed. It removes the window from the ECS world and relevant displays.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the ID of the destroyed window.
/// * `windows` - A query for all windows with their parent.
/// * `apps` - A query for all applications.
/// * `displays` - A query for all displays.
/// * `commands` - Bevy commands to despawn entities and trigger events.
#[allow(clippy::needless_pass_by_value)]
fn window_destroyed_trigger(
    trigger: On<WMEventTrigger>,
    mut windows: Query<(&Window, Entity, &ChildOf)>,
    mut apps: Query<&mut Application>,
    mut displays: Query<&mut Display>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let Event::WindowDestroyed { window_id } = trigger.event().0 else {
        return;
    };

    let Some((window, entity, child)) = windows
        .iter()
        .find(|(window, _, _)| window.id() == window_id)
    else {
        error!(
            "{}: Trying to destroy non-existing window {window_id}.",
            function_name!()
        );
        return;
    };

    let Ok(mut app) = apps.get_mut(child.parent()) else {
        error!(
            "{}: Window {} has no parent!",
            function_name!(),
            window.id()
        );
        return;
    };
    app.unobserve_window(window);
    commands.entity(entity).despawn();

    let mut lens = windows.transmute_lens::<&Window>();
    for mut display in &mut displays {
        let Ok(panel) = window_manager
            .active_display_space(display.id())
            .and_then(|active_space| display.active_panel(active_space))
        else {
            continue;
        };

        give_away_focus(entity, &lens.query(), panel, &mut commands);
        display.remove_window(entity);
    }
}

/// Moves the focus away to a neighbour window.
fn give_away_focus(
    entity: Entity,
    windows: &Query<&Window>,
    active_pane: &mut WindowPane,
    commands: &mut Commands,
) {
    // Move focus to a left neighbour if the panel has more windows.
    if let Ok(index) = active_pane.index_of(entity)
        && active_pane.len() > 1
    {
        let neighbour = active_pane.get(index.saturating_sub(1)).ok();

        if let Some(window) = neighbour
            .and_then(|pane| pane.top())
            .and_then(|entity| windows.get(entity).ok())
        {
            let window_id = window.id();
            debug!(
                "{}: window destroyed, moving focus to {window_id}",
                function_name!()
            );
            commands.trigger(WMEventTrigger(Event::WindowFocused { window_id }));
            commands.trigger(ReshuffleAroundTrigger(window_id));
        }
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
fn spawn_window_trigger(
    mut trigger: On<SpawnWindowTrigger>,
    windows: Query<(Entity, &Window, Has<FocusedMarker>)>,
    mut apps: Query<(Entity, &mut Application)>,
    mut active_display: Single<&mut Display, With<ActiveDisplayMarker>>,
    config: Configuration,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let new_windows = &mut trigger.event_mut().0;

    while let Some(mut window) = new_windows.pop() {
        let window_id = window.id();

        if windows
            .iter()
            .any(|(_, window, _)| window.id() == window_id)
        {
            continue;
        }

        debug!("{}: window {}", function_name!(), window_id);
        let Ok(pid) = ax_window_pid(&window.element()) else {
            warn!(
                "{}: Unable to get window pid for {}",
                function_name!(),
                window_id,
            );
            continue;
        };
        let Some((app_entity, mut app)) = apps.iter_mut().find(|(_, app)| app.pid() == pid) else {
            warn!(
                "{}: unable to find application with pid {pid}.",
                function_name!()
            );
            continue;
        };

        debug!(
            "{}: created {} title: {} role: {} subrole: {} element: {}",
            function_name!(),
            window_id,
            window.title().unwrap_or_default(),
            window.role().unwrap_or_default(),
            window.subrole().unwrap_or_default(),
            window.element(),
        );

        if app.observe_window(&window).is_err() {
            warn!(
                "{}: Error observing window {}.",
                function_name!(),
                window_id
            );
        }
        window.psn = Some(app.psn());
        window.eligible = app.parent_window(active_display.id()).is_err() || window.is_root();
        let bundle_id = app.bundle_id().map(String::as_str).unwrap_or_default();
        debug!(
            "{}: window {} isroot {} eligible {} bundle_id {}",
            function_name!(),
            window_id,
            window.is_root(),
            window.is_eligible(),
            bundle_id,
        );

        let title = window.title().unwrap_or_default();
        let properties = config
            .find_window_properties(&title, bundle_id)
            .inspect(|_| {
                debug!(
                    "{}: Applying window properties for '{title}",
                    function_name!()
                );
            });
        apply_window_properties(
            window,
            app_entity,
            properties.as_ref(),
            &mut active_display,
            &windows,
            &window_manager,
            &mut commands,
        );
    }
}

fn apply_window_properties(
    mut window: Window,
    app_entity: Entity,
    properties: Option<&WindowParams>,
    active_display: &mut Display,
    windows: &Query<(Entity, &Window, Has<FocusedMarker>)>,
    window_manager: &WindowManager,
    commands: &mut Commands,
) {
    let window_id = window.id();
    let floating = properties
        .as_ref()
        .and_then(|props| props.floating)
        .unwrap_or(false);
    let wanted_insertion = properties.as_ref().and_then(|props| props.index);
    window.manage(!floating);
    _ = window
        .update_frame(Some(&active_display.bounds))
        .inspect_err(|err| error!("{}: {err}", function_name!()));

    // Insert the window into the internal Bevy state.
    let entity = commands.spawn((window, ChildOf(app_entity))).id();

    if floating {
        // Avoid managing window if it's floating.
        return;
    }

    let Ok(panel) = window_manager
        .active_display_space(active_display.id())
        .and_then(|active_space| active_display.active_panel(active_space))
    else {
        return;
    };

    // Attempt inserting the window at a pre-defined position.
    let insert_at = wanted_insertion.map_or_else(
        || {
            // Otherwise attempt inserting it after the current focus.
            let focused_window = windows
                .iter()
                .find_map(|(entity, _, focused)| focused.then_some(entity));
            // Insert to the right of the currently focused window
            focused_window
                .and_then(|entity| panel.index_of(entity).ok())
                .and_then(|insert_at| (insert_at + 1 < panel.len()).then_some(insert_at + 1))
        },
        Some,
    );

    debug!("{}: New window adding at {panel}", function_name!());
    match insert_at {
        Some(after) => {
            debug!("{}: New window inserted at {after}", function_name!());
            panel.insert_at(after, entity);
        }
        None => panel.append(entity),
    }

    commands.trigger(ReshuffleAroundTrigger(window_id));
}

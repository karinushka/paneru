use bevy::app::AppExit;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::{ChildOf, Children};
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::query::{Changed, Has, Or, With, Without};
use bevy::ecs::system::{
    Commands, Local, NonSend, NonSendMut, ParallelCommands, Populated, Query, Res, Single,
};
use bevy::math::IRect;
use bevy::tasks::AsyncComputeTaskPool;
use bevy::tasks::futures_lite::future;
use bevy::time::Time;
use objc2_core_graphics::CGDirectDisplayID;
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use tracing::{Level, debug, error, info, instrument, trace, warn};

use super::{
    ActiveDisplayMarker, BProcess, ExistingMarker, FocusedMarker, FreshMarker,
    PollForNotifications, RepositionMarker, ResizeMarker, SpawnWindowTrigger, Timeout,
    WMEventTrigger,
};
use crate::config::{BorderRadiusOption, Config, SwipeGestureDirection};
use crate::ecs::params::{ActiveDisplay, Configuration, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, BruteforceWindows, DockPosition, Initializing,
    LocateDockTrigger, Position, ReshuffleAroundMarker, Scrolling, StackAdjustedResize, Unmanaged,
    WidthRatio, WindowDraggedMarker, reposition_entity, reshuffle_around, resize_entity,
};
use crate::events::Event;
use crate::manager::{
    Application, Column, Display, LayoutStrip, Origin, Process, Size, Window, WindowManager,
    WindowOS, bruteforce_windows,
};
use crate::overlay::OverlayManager;
use crate::platform::{PlatformCallbacks, WorkspaceId};

/// Processes a single incoming `Event`. It dispatches various event types to the `WindowManager` or other internal handlers.
/// This system reads `Event` messages and triggers appropriate Bevy events or modifies resources based on the event type.
///
/// # Arguments
///
/// * `messages` - A `MessageReader` for incoming `Event` messages.
/// * `broken_notifications` - A mutable `ResMut` for the `PollForNotifications` resource, used to manage polling state.
/// * `commands` - Bevy commands to trigger events or insert resources.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn dispatch_toplevel_triggers(
    mut messages: MessageReader<Event>,
    broken_notifications: Option<Res<PollForNotifications>>,
    mut commands: Commands,
) {
    for event in messages.read() {
        match event {
            Event::WindowCreated { element } => {
                if let Ok(window) = WindowOS::new(element)
                    .inspect_err(|err| {
                        trace!("not adding window {element:?}: {err}");
                    })
                    .map(|window| Window::new(Box::new(window)))
                {
                    commands.trigger(SpawnWindowTrigger(vec![window]));
                }
            }

            Event::SpaceChanged => {
                if broken_notifications.is_some() {
                    info!(
                        "Workspace and display notifications arriving correctly. Disabling the polling.",
                    );
                    commands.remove_resource::<PollForNotifications>();
                }
                commands.trigger(WMEventTrigger(event.clone()));
            }

            Event::WindowTitleChanged { window_id } => {
                trace!("WindowTitleChanged: {window_id:?}");
            }
            Event::MenuClosed { window_id } => {
                trace!("MenuClosed event: {window_id:?}");
            }
            Event::DisplayResized { display_id } => {
                debug!("Display Resized: {display_id:?}");
            }
            Event::DisplayConfigured { display_id } => {
                debug!("Display Configured: {display_id:?}");
            }
            Event::SystemWoke { msg } => {
                debug!("system woke: {msg:?}");
            }

            _ => commands.trigger(WMEventTrigger(event.clone())),
        }
    }
}

/// Gathers all present displays and spawns them as entities in the Bevy world.
/// The currently active display (identified by `window_manager.active_display_id()`) is marked with `ActiveDisplayMarker`.
///
/// # Arguments
///
/// * `window_manager` - The `WindowManager` resource for querying display information.
/// * `commands` - Bevy commands to spawn entities.
#[allow(clippy::needless_pass_by_value)]
pub fn gather_displays(window_manager: Res<WindowManager>, mut commands: Commands) {
    let Ok(active_display_id) = window_manager.active_display_id() else {
        error!("Unable to get active display id!");
        return;
    };
    for (display, workspaces) in window_manager.present_displays() {
        let origin = Position(display.bounds().min);
        let entity = if display.id() == active_display_id {
            commands.spawn((display, ActiveDisplayMarker))
        } else {
            commands.spawn(display)
        }
        .id();

        commands.trigger(LocateDockTrigger(entity));

        let Ok(active_space) = window_manager.active_display_space(active_display_id) else {
            return;
        };

        for id in workspaces {
            let strip = LayoutStrip::new(id);
            if id == active_space {
                commands.spawn((
                    strip,
                    origin.clone(),
                    Scrolling::default(),
                    ActiveWorkspaceMarker,
                    ChildOf(entity),
                ));
            } else {
                commands.spawn((strip, origin.clone(), Scrolling::default(), ChildOf(entity)));
            }
        }
    }
}

/// Adds an existing process to the window manager. This is used during initial setup for already running applications.
/// It attempts to create a new `Application` instance from the `BProcess` and attaches it as a child entity.
/// The `ExistingMarker` is then removed from the process entity.
///
/// # Arguments
///
/// * `window_manager` - The `WindowManager` resource for creating new application instances.
/// * `process_query` - A query for existing `BProcess` entities marked with `ExistingMarker`.
/// * `commands` - Bevy commands to spawn entities and manage components.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(crate) fn add_existing_process(
    window_manager: Res<WindowManager>,
    processes: Populated<(Entity, &BProcess), With<ExistingMarker>>,
    mut commands: Commands,
) {
    for (entity, process) in processes {
        let Ok(app) = window_manager.new_application(&*process.0) else {
            error!("creating aplication from process '{}'", process.name());
            return;
        };
        commands.spawn((app, ExistingMarker, ChildOf(entity)));
        commands.entity(entity).try_remove::<ExistingMarker>();
    }
}

/// Adds an existing application to the window manager. This is used during initial setup.
/// It observes the application, adds its windows to the manager, and then triggers `SpawnWindowTrigger` events for newly found windows.
/// The `ExistingMarker` is removed from the application entity after processing.
///
/// # Arguments
///
/// * `window_manager` - The `WindowManager` resource for interacting with window management logic.
/// * `displays` - A query for all `Display` entities, used to gather all existing space IDs.
/// * `app_query` - A query for existing `Application` entities marked with `ExistingMarker`.
/// * `commands` - Bevy commands to spawn entities and manage components.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(crate) fn add_existing_application(
    window_manager: Res<WindowManager>,
    workspaces: Query<&LayoutStrip>,
    fresh_apps: Populated<(&mut Application, Entity), With<ExistingMarker>>,
    mut commands: Commands,
) {
    let spaces = workspaces
        .into_iter()
        .map(LayoutStrip::id)
        .collect::<Vec<_>>();
    let thread_pool = AsyncComputeTaskPool::get();

    for (mut app, entity) in fresh_apps {
        let mut offscreen_windows = vec![];

        if app.observe().is_ok_and(|result| result)
            && let Ok((found_windows, offscreen)) = window_manager
                .find_existing_application_windows(&mut app, &spaces)
                .inspect_err(|err| warn!("{err}"))
        {
            offscreen_windows.extend(offscreen);
            commands.trigger(SpawnWindowTrigger(found_windows));
        }
        commands.entity(entity).try_remove::<ExistingMarker>();

        if !offscreen_windows.is_empty() {
            let pid = app.pid();
            let bruteforce_task =
                thread_pool.spawn(async move { bruteforce_windows(pid, offscreen_windows) });
            commands.spawn(BruteforceWindows(bruteforce_task));
        }
    }
}

/// Finishes the initialization process once all initial windows are loaded.
/// This system refreshes displays, assigns the `FocusedMarker` to the first window of the active space,
/// and logs the total number of managed windows.
///
/// # Arguments
///
/// * `windows` - A mutable query for all `Window` components, their `Entity`, and `Has<Unmanaged>` status.
/// * `displays` - A query for all `Display` entities, including whether they have the `ActiveDisplayMarker`.
/// * `window_manager` - The `WindowManager` resource for refreshing displays and getting active space information.
/// * `commands` - Bevy commands to insert components like `FocusedMarker`.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(crate) fn finish_setup(
    process_query: Query<Entity, With<ExistingMarker>>,
    windows: Windows,
    apps: Query<&Application>,
    mut bruteforce_tasks: Query<(Entity, &mut BruteforceWindows)>,
    mut workspaces: Query<(&mut LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    if !process_query.is_empty() {
        // The other two add_* functions are still running..
        return;
    }

    // Reap the bruteforced windows.
    if !bruteforce_tasks.is_empty() {
        for (entity, mut job) in &mut bruteforce_tasks {
            if let Some(found_windows) = future::block_on(future::poll_once(&mut job.0)) {
                commands.trigger(SpawnWindowTrigger(found_windows));
                commands.entity(entity).despawn();
            }
        }
        // Wait for the next tick to finish initialization.
        return;
    }

    info!(
        "Initialization: found {:?} windows.",
        windows.iter().size_hint()
    );

    for (mut strip, active_strip) in &mut workspaces {
        debug!("space {}: before refresh {strip:?}", strip.id());
        let workspace_windows = window_manager
            .windows_in_workspace(strip.id())
            .inspect_err(|err| {
                warn!("failed to get windows on workspace {}: {err}", strip.id());
            })
            .ok()
            .map(|workspace_windows| {
                workspace_windows
                    .into_iter()
                    .filter_map(|window_id| windows.find_managed(window_id))
                    .filter(|(window, entity)| {
                        if window.is_minimized() {
                            commands.entity(*entity).try_insert(Unmanaged::Minimized);
                            false
                        } else {
                            true
                        }
                    })
                    .collect::<Vec<_>>()
            });
        let Some(workspace_windows) = workspace_windows else {
            continue;
        };

        // Preserve the order - do not flush existing windows.
        for entity in strip.all_windows() {
            if !workspace_windows.iter().any(|(_, e)| *e == entity) {
                strip.remove(entity);
            }
        }
        for (_, entity) in workspace_windows {
            if strip.index_of(entity).is_err() {
                strip.append(entity);
            }
        }
        debug!("space {}: after refresh {strip:?}", strip.id());

        if active_strip && let Some(entity) = strip.first().ok().and_then(|column| column.top()) {
            reshuffle_around(entity, &mut commands);
            commands.entity(entity).try_insert(FocusedMarker);
            if let Some(window) = windows.get(entity)
                && let Some(psn) = windows.psn(window.id(), &apps)
            {
                debug!("raising {}", window.id());
                window.focus_with_raise(psn);
            }
        }
    }

    commands.remove_resource::<Initializing>();
}

/// Handles the event when a new application is launched. It creates a `Process` and `Application` object,
/// observes the application for events, and adds its windows to the manager.
/// This system processes `BProcess` entities marked with `FreshMarker`.
/// If the process is not yet ready, it continues observing it. If ready, it attempts to create and observe an `Application`.
/// A `Timeout` is added to the application if it takes too long to become observable.
///
/// # Arguments
///
/// * `window_manager` - The `WindowManager` resource for creating new application instances.
/// * `process_query` - A `Populated` query for `(Entity, &mut BProcess, Has<Children>)` with `With<FreshMarker>`.
/// * `commands` - Bevy commands to spawn entities and manage components.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn add_launched_process(
    window_manager: Res<WindowManager>,
    fresh_processes: Populated<(Entity, &mut BProcess, Has<Children>), With<FreshMarker>>,
    mut commands: Commands,
) {
    const APP_OBSERVABLE_TIMEOUT_SEC: u64 = 5;
    let mut already_seen = HashSet::new();

    for (entity, mut process, children) in fresh_processes {
        let process = &mut *process.0;

        if !already_seen.insert(process.psn()) {
            continue;
        }

        if !process.ready() {
            continue;
        }

        if children {
            // Process already has an attached Application, so finish.
            commands.entity(entity).try_remove::<FreshMarker>();
            continue;
        }

        let Ok(mut app) = window_manager.new_application(process) else {
            error!("creating aplication from process '{}'", process.name());
            return;
        };

        if app.observe().is_ok_and(|good| good) {
            let timeout = Timeout::new(
                Duration::from_secs(APP_OBSERVABLE_TIMEOUT_SEC),
                Some(format!(
                    "{app} did not become observable in {APP_OBSERVABLE_TIMEOUT_SEC}s.",
                )),
            );
            commands.spawn((app, FreshMarker, timeout, ChildOf(entity)));
        } else {
            debug!("failed to register some observers {}", process.name());
        }
    }
}

/// Adds windows for a newly launched application.
/// This system processes `Application` entities marked with `FreshMarker`.
/// It queries the application's window list, filters out already existing windows, and triggers `SpawnWindowTrigger` events for new windows.
/// The `FreshMarker` is removed from the application entity after processing.
///
/// # Arguments
///
/// * `app_query` - A `Populated` query for `(&mut Application, Entity)` with `With<FreshMarker>`.
/// * `windows` - A query for all `Window` components, used to check for existing windows.
/// * `commands` - Bevy commands to spawn entities and manage components.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn add_launched_application(
    app_query: Populated<(&mut Application, Entity, Has<Children>), With<FreshMarker>>,
    windows: Windows,
    mut commands: Commands,
) {
    // TODO: maybe refactor this with add_existing_application_windows()
    let find_window = |window_id| windows.find(window_id);

    for (app, entity, has_children) in app_query {
        let mut create_windows = app.window_list();
        // Retain the non-existing windows, so they can be created.
        create_windows.retain(|window| find_window(window.id()).is_none());

        if !create_windows.is_empty() {
            commands.entity(entity).try_remove::<FreshMarker>();
            debug!(
                "spawn! (polling path found {} new windows for {entity})",
                create_windows.len(),
            );
            commands.trigger(SpawnWindowTrigger(create_windows));
        } else if has_children {
            // Windows were already created via AXCreated notification path.
            // Remove FreshMarker so the Timeout gets cleaned up.
            debug!("removing FreshMarker from {entity}: windows already created via AXCreated");
            commands.entity(entity).try_remove::<FreshMarker>();
        }
    }
}

/// Cleans up entities which have been initializing for too long, specifically `BProcess` or `Application` entities.
/// This system removes the `Timeout` component from entities that are no longer `Fresh`.
///
/// This can be processes which are not yet observable or applications which keep failing to
/// register some of the observers.
///
/// # Arguments
///
/// * `cleanup` - A `Populated` query for `(Entity, Has<FreshMarker>, &Timeout)` components, targeting `BProcess` or `Application` entities.
/// * `commands` - Bevy commands to remove components.
#[allow(clippy::type_complexity)]
pub(super) fn fresh_marker_cleanup(
    cleanup: Populated<
        (Entity, Has<FreshMarker>, &Timeout),
        Or<(With<BProcess>, With<Application>)>,
    >,
    mut commands: Commands,
) {
    for (entity, fresh, _) in cleanup {
        if !fresh {
            // Process was ready before the timer finished.
            commands.entity(entity).try_remove::<Timeout>();
        }
    }
}

/// A Bevy system that ticks `Timeout` timers and despawns entities when their timers finish.
/// This system is responsible for cleaning up entities that have exceeded their allotted time for an operation.
///
/// # Arguments
///
/// * `timers` - A `Populated` query for `(Entity, &mut Timeout)` components.
/// * `clock` - The Bevy `Time` resource for getting the delta time.
/// * `commands` - Bevy commands to despawn entities.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn timeout_ticker(
    timers: Populated<(Entity, &mut Timeout)>,
    clock: Res<Time>,
    mut commands: Commands,
) {
    for (entity, mut timeout) in timers {
        if timeout.timer.is_finished() {
            trace!("Despawning entity {entity} due to timeout.");
            if let Some(message) = &timeout.message {
                debug!("{message}");
            }
            trace!("Removing timer {entity}");
            commands.entity(entity).despawn();
        } else {
            trace!("Timer {}", timeout.timer.elapsed().as_secs_f32());
            timeout.timer.tick(clock.delta());
        }
    }
}

/// Re-parents orphaned workspace strips to the display that currently owns their space.
///
/// When a display is removed, its strips are detached (orphaned). macOS moves the spaces
/// to remaining displays. This system queries `present_displays()` to find which display
/// now owns each orphaned space and re-parents the strip directly to that display.
///
/// # Arguments
///
/// * `orphans` - A `Populated` query for `LayoutStrip` entities without a parent display.
/// * `displays` - A query for all `Display` entities.
/// * `windows` - A `Windows` system parameter for accessing window components.
/// * `window_manager` - The `WindowManager` resource for querying current display/space assignments.
/// * `commands` - Bevy commands to re-parent entities and remove timeouts.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn find_orphaned_workspaces(
    orphans: Populated<(&LayoutStrip, Entity, &Timeout), Without<ChildOf>>,
    mut attached: Query<(&mut LayoutStrip, &ChildOf), With<ChildOf>>,
    displays: Query<(&Display, Entity)>,
    windows: Windows,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let present = window_manager.present_displays();

    for (orphan, orphan_entity, timeout) in orphans {
        if timeout.timer.is_finished() {
            // Rescue windows from orphaned strips before despawning by floating them.
            debug!("Rescue windows from timed out orphan {}.", orphan.id());
            for lost_window in orphan.all_windows() {
                if let Ok(mut cmd) = commands.get_entity(lost_window) {
                    cmd.try_insert(Unmanaged::Floating);
                }
            }
            continue;
        }

        // Find which display now owns this space ID.
        let target = present.iter().find_map(|(present_display, spaces)| {
            if spaces.iter().any(|&id| id == orphan.id()) {
                displays
                    .iter()
                    .find(|(d, _)| d.id() == present_display.id())
            } else {
                None
            }
        });
        let Some((target_display, target_entity)) = target else {
            continue; // No display owns this space yet; wait for next tick.
        };

        if !attached
            .iter()
            .any(|(_, child)| child.parent() == target_entity)
        {
            // This display has no spaces attached yet - so it may pick up the orphans in this
            // tick. So wait until next tick.
            continue;
        }

        debug!(
            "Re-parenting orphaned strip {} to display {}",
            orphan.id(),
            target_display.id(),
        );

        let all_windows = orphan.all_windows();
        if let Some((mut target_strip, _)) = attached
            .iter_mut()
            .find(|(strip, child)| child.parent() == target_entity && strip.id() == orphan.id())
        {
            // Move windows into existing workspace strip.
            debug!("moving windows into existing layout strip.");
            for entity in orphan.all_windows() {
                target_strip.append(entity);
            }
        } else {
            // Display does not have this strip, add it.
            debug!("adding the layout strip directly.");
            if let Ok(mut commands) = commands.get_entity(orphan_entity) {
                commands
                    .try_remove::<Timeout>()
                    .insert(ChildOf(target_entity));
            }
        }

        refresh_workspace_window_sizes(
            orphan.id(),
            &all_windows,
            &windows,
            target_display,
            &window_manager,
            &mut commands,
        );

        if let Ok(mut cmd) = commands.get_entity(orphan_entity) {
            cmd.despawn();
        }
    }
}

fn refresh_workspace_window_sizes(
    space_id: WorkspaceId,
    orphans: &[Entity],
    windows: &Windows,
    display: &Display,
    window_manager: &WindowManager,
    commands: &mut Commands,
) {
    let mut in_workspace = window_manager
        .windows_in_workspace(space_id)
        .inspect_err(|err| {
            warn!("getting windows in workspace: {err}");
        })
        .unwrap_or_default();
    let viewport = display.bounds();

    // Resize windows for the new display dimensions.
    for &entity in orphans {
        if let Some(window) = windows.get(entity) {
            let width_ratio = windows.width_ratio(entity).unwrap_or(0.5);
            let size = Size::new(
                (f64::from(viewport.width()) * width_ratio) as i32,
                viewport.height(),
            );
            debug!(
                "refreshing ratio {:.1} for window {}: {size}",
                width_ratio,
                window.id(),
            );
            resize_entity(entity, size, commands);
            in_workspace.retain(|window_id| *window_id != window.id());
        }
    }

    // Find remaining windows which are outside of the strip.                                                  ...
    let floating = in_workspace.into_iter().filter_map(|window_id| {
        windows
            .find(window_id)
            .and_then(|(_, entity)| windows.get_managed(entity))
            .and_then(|(_, entity, unmanaged)| {
                matches!(unmanaged, Some(Unmanaged::Floating)).then_some(entity)
            })
    });
    for window_entity in floating {
        debug!("repositioning floating window {window_entity}");
        reposition_entity(window_entity, viewport.min, commands);
    }
}

/// Periodically checks for displays added and removed, as well as changes in the active display.
/// This system acts as a workaround for inconsistent display change notifications on some macOS versions.
/// It uses `ThrottledSystem` to limit its execution frequency.
///
/// # Arguments
///
/// * `displays` - A query for all `Display` entities, including whether they have the `ActiveDisplayMarker`.
/// * `window_manager` - The `WindowManager` resource for querying active display information.
/// * `throttle` - A `ThrottledSystem` to control the execution rate of this system.
/// * `commands` - Bevy commands to trigger `WMEventTrigger` events for display changes.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn display_changes_watcher(
    displays: Query<(&Display, Has<ActiveDisplayMarker>)>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let Ok(current_display_id) = window_manager.active_display_id() else {
        return;
    };
    let found = displays
        .iter()
        .find(|(display, _)| display.id() == current_display_id);
    if let Some((_, active)) = found {
        if active {
            return;
        }
        debug!("detected dislay change from {current_display_id}.");
        commands.trigger(WMEventTrigger(Event::DisplayChanged));
    } else {
        debug!("new display {current_display_id} detected.");
        commands.trigger(WMEventTrigger(Event::DisplayAdded {
            display_id: current_display_id,
        }));
    }

    let present_displays = window_manager.present_displays();
    displays.iter().for_each(|(display, _)| {
        if !present_displays
            .iter()
            .any(|(present_display, _)| present_display.id() == display.id())
        {
            let display_id = display.id();
            debug!("detected removal of display {display_id}");
            commands.trigger(WMEventTrigger(Event::DisplayRemoved {
                display_id: display.id(),
            }));
        }
    });
}

/// Periodically checks for changes in the active workspace (space) on the active display.
/// This system acts as a workaround for inconsistent workspace change notifications on some macOS versions.
/// If a change is detected, it triggers an `Event::SpaceChanged` event.
///
/// # Arguments
///
/// * `active_display` - An `ActiveDisplay` system parameter providing immutable access to the active display.
/// * `window_manager` - The `WindowManager` resource for querying active space information.
/// * `throttle` - A `ThrottledSystem` to control the execution rate of this system.
/// * `current_space` - A `Local` resource storing the ID of the currently observed space.
/// * `commands` - Bevy commands to trigger `WMEventTrigger` events for space changes.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn workspace_change_watcher(
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    mut current_space: Local<WorkspaceId>,
    mut commands: Commands,
) {
    let Ok(space_id) = window_manager
        .0
        .active_display_space(active_display.id())
        .inspect_err(|err| warn!("{err}"))
    else {
        return;
    };

    if *current_space != space_id {
        *current_space = space_id;
        debug!("workspace changed to {space_id}");
        commands.trigger(WMEventTrigger(Event::SpaceChanged));
    }
}

/// Animates window movement.
/// This is a Bevy system that runs on `Update`. It smoothly moves windows to their target
/// positions, as indicated by the `RepositionMarker` component.
/// Animation speed is controlled by the `animation_speed` in the `Config`.
/// When a window reaches its target position, the `RepositionMarker` is removed.
///
/// # Arguments
///
/// * `windows` - A `Populated` query for `(&mut Window, Entity, &RepositionMarker)` components.
/// * `displays` - A query for all `Display` entities, used to get display bounds and menubar height.
/// * `time` - The Bevy `Time` resource for calculating delta time.
/// * `config` - The `Config` resource, used for animation speed.
/// * `commands` - Bevy commands to remove the `RepositionMarker` when animation is complete.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn animate_entities(
    mut animate: Populated<(&mut Position, Entity, &RepositionMarker)>,
    active_display: ActiveDisplay,
    time: Res<Time>,
    config: Res<Config>,
    commands: ParallelCommands,
) {
    let move_ratio = config.animation_speed() * time.delta_secs_f64();
    let move_delta = move_ratio * f64::from(active_display.display().width());

    animate
        .par_iter_mut()
        .for_each(|(mut position, entity, RepositionMarker(origin))| {
            let delta = position
                .0
                .as_vec2()
                .move_towards(origin.as_vec2(), move_delta as f32)
                .as_ivec2();

            trace!(
                "entity {entity} source {} dest {origin} delta {move_delta} moving to {delta}",
                position.0,
            );
            position.0 = delta;
            if *origin == delta {
                commands.command_scope(|mut command| {
                    command.entity(entity).try_remove::<RepositionMarker>();
                });
            }
        });
}

/// Animates window resizing.
/// This is a Bevy system that runs on `Update`. It resizes windows to their target
/// dimensions, as indicated by the `ResizeMarker` component.
/// When a window reaches its target size, the `ResizeMarker` is removed.
///
/// # Arguments
///
/// * `windows` - A `Populated` query for `(&mut Window, Entity, &ResizeMarker)` components.
/// * `active_display` - An `ActiveDisplay` system parameter providing immutable access to the active display.
/// * `commands` - Bevy commands to remove the `ResizeMarker` when resizing is complete.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn animate_resize_entities(
    mut animate: Populated<(&mut Bounds, Entity, &ResizeMarker, Has<RepositionMarker>)>,
    active_display: ActiveDisplay,
    time: Res<Time>,
    config: Res<Config>,
    commands: ParallelCommands,
) {
    let move_ratio = config.animation_speed() * time.delta_secs_f64();
    let move_delta = move_ratio * f64::from(active_display.display().width());

    animate
        .par_iter_mut()
        .for_each(|(mut bounds, entity, ResizeMarker(size), moving)| {
            if moving {
                // Defer resize while the window is being repositioned so it doesn't extend past
                // the screen edge before the move lands.
                // Exception: when the resize *shrinks* the window (e.g. stacking), there is no
                // risk of overshooting the screen, and deferring would leave the window at its old
                // (full) height until the reposition finishes.
                let current_size = bounds.0;
                if size.x > current_size.x || size.y > current_size.y {
                    return;
                }
            }

            let delta = bounds
                .0
                .as_vec2()
                .move_towards(size.as_vec2(), move_delta as f32)
                .as_ivec2();

            trace!(
                "entity {entity} source {} dest {size} delta {move_delta} resizing to {delta}",
                bounds.0,
            );
            bounds.0 = delta;
            if *size == delta {
                commands.command_scope(|mut command| {
                    command.entity(entity).try_remove::<ResizeMarker>();
                });
            }
        });
}

#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn apply_scroll_physics(
    mut active_workspace: Single<
        (Entity, &LayoutStrip, &mut Position, &mut Scrolling),
        (With<ActiveWorkspaceMarker>, Without<Window>),
    >,
    active_display: Single<(&Display, Option<&DockPosition>), With<ActiveDisplayMarker>>,
    windows: Windows,
    window_manager: Res<WindowManager>,
    mut config: Configuration,
    time: Res<Time>,
    mut commands: Commands,
) {
    const FOCUS_VELOCITY_RATIO: f64 = 0.3;
    const FINGER_LIFT_THRESHOLD: Duration = Duration::from_millis(50);
    const MIN_VELOCITY_PX: f64 = 100.0;

    let (entity, strip, ref mut position, ref mut scroll) = *active_workspace;
    let dt = time.delta_secs_f64();

    // Finger lift detection
    if scroll.is_user_swiping && scroll.last_event.elapsed() > FINGER_LIFT_THRESHOLD {
        scroll.is_user_swiping = false;
    }

    // While user is swiping, velocity is directly applied in the trigger.
    // We just need to update the position.
    let display_width = f64::from(active_display.0.bounds().width());
    let scroll_velocity = scroll.velocity.abs() * display_width;
    if !scroll.is_user_swiping {
        if scroll_velocity < FOCUS_VELOCITY_RATIO * display_width
            && config.focus_follows_mouse()
            && let Some(point) = window_manager.cursor_position()
        {
            config.set_ffm_flag(None);
            commands.trigger(WMEventTrigger(Event::MouseMoved { point }));
        }

        if scroll_velocity < MIN_VELOCITY_PX {
            // Below threshold: stop and focus
            if let Ok(mut cmd) = commands.get_entity(entity) {
                cmd.try_remove::<Scrolling>();
            }
            return;
        }
        // Apply inertia decay
        let decay_rate = config.config().swipe_deceleration();
        scroll.velocity *= (-decay_rate * dt).exp();
    }

    let get_window_frame = |entity| get_moving_window_frame(entity, &windows);
    let viewport = active_display
        .0
        .actual_display_bounds(active_display.1, config.config());

    let absolute_positions = strip
        .absolute_positions(&get_window_frame)
        .collect::<Vec<_>>();

    // Apply soft-snap to center during inertia.
    if !scroll.is_user_swiping
        && config.auto_center()
        && let Some((velocity, snap_offset)) = magnetic_snap_to_center(
            dt,
            &viewport,
            position.x,
            &absolute_positions,
            &get_window_frame,
            scroll,
        )
    {
        scroll.velocity = velocity;
        position.x -= snap_offset;
        return;
    }

    let frame_delta = scroll.velocity * dt;
    let shift = (f64::from(viewport.width()) * frame_delta) as i32;

    if let Some(clamped_offset) = clamp_viewport_offset(
        position.x,
        shift,
        &absolute_positions,
        &get_window_frame,
        &viewport,
        config.config(),
    ) {
        position.x = clamped_offset;
    } else {
        scroll.velocity = 0.0;
    }
}

fn magnetic_snap_to_center<W>(
    time_delta: f64,
    viewport: &IRect,
    current_position: i32,
    absolute_positions: &[(&Column, i32)],
    get_window_frame: &W,
    scroll: &Scrolling,
) -> Option<(f64, i32)>
where
    W: Fn(Entity) -> Option<IRect>,
{
    const CENTER_MAGNETIC_PULL: f64 = 0.8;
    const CENTER_MAGNETIC_FORCE: f64 = 4.0;
    // Use 5% of the display width as the snap threshold
    const SNAP_DISPLAY_RATIO: f64 = 0.05;

    let viewport_center = viewport.center().x;
    let target_offset = absolute_positions
        .iter()
        .map(|(column, pos)| {
            let col_width = column
                .top()
                .and_then(&get_window_frame)
                .map_or(0, |f| f.width());
            viewport_center - (pos + col_width / 2)
        })
        .min_by_key(|target| (current_position - target).abs())
        .unwrap_or(current_position);

    let snap_threshold = SNAP_DISPLAY_RATIO * f64::from(viewport.width());
    let dist_to_snap = f64::from(current_position - target_offset);

    if dist_to_snap.abs() > snap_threshold {
        return None;
    }

    // Magnetic pull: slow down and nudge towards center.
    Some((
        scroll.velocity * CENTER_MAGNETIC_PULL,
        (dist_to_snap * time_delta * CENTER_MAGNETIC_FORCE) as i32,
    ))
}

#[instrument(level = Level::TRACE, skip_all, fields(current_offset, shift), ret)]
fn clamp_viewport_offset<W>(
    current_offset: i32,
    shift: i32,
    absolute_positions: &[(&Column, i32)],
    get_window_frame: &W,
    viewport: &IRect,
    config: &Config,
) -> Option<i32>
where
    W: Fn(Entity) -> Option<IRect>,
{
    let swipe_direction_modifier = match config.swipe_gesture_direction() {
        SwipeGestureDirection::Natural => 1,
        SwipeGestureDirection::Reversed => -1,
    };
    let shift = shift * swipe_direction_modifier;

    let total_strip_width = absolute_positions.last().and_then(|(column, offset)| {
        column
            .top()
            .and_then(|entity| get_window_frame(entity).map(|frame| offset + frame.width()))
    })?;

    // Continous swipe is on by default.
    let continuous_swipe = config.options().continuous_swipe.is_none_or(|swipe| swipe);
    let snap_sides = absolute_positions
        .last()
        .map(|(_, pos)| pos)
        .zip(absolute_positions.get(1).map(|(_, pos)| pos));
    Some(
        if continuous_swipe && let Some((left_snap, right_snap)) = snap_sides {
            // Allow to scroll away until the last or first window snaps.
            (current_offset - shift).clamp(viewport.min.x - left_snap, viewport.max.x - right_snap)
        } else if viewport.width() < total_strip_width {
            // Snap the strip directly to the edges.
            (current_offset - shift).clamp(viewport.max.x - total_strip_width, viewport.min.x)
        } else {
            // Snap the strip directly to the edges.
            (current_offset - shift).clamp(viewport.min.x, viewport.max.x - total_strip_width)
        },
    )
}

fn expose_window(
    entity: Entity,
    windows: &Windows,
    active_display: &ActiveDisplay,
    config: &Config,
) -> Option<IRect> {
    let display_bounds = active_display
        .display()
        .actual_display_bounds(active_display.dock(), config);
    let mut frame = get_moving_window_frame(entity, windows)?;
    let size = frame.size();

    if frame.max.x > display_bounds.max.x {
        trace!("Bumped window {entity} to the left");
        frame.min.x = display_bounds.max.x - size.x;
    } else if frame.min.x < display_bounds.min.x {
        trace!("Bumped window {entity} to the right");
        frame.min.x = display_bounds.min.x;
    }
    frame.max.x = frame.min.x + size.x;
    Some(frame)
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn reshuffle_layout_strip(
    marker: Populated<Entity, With<ReshuffleAroundMarker>>,
    active_display: ActiveDisplay,
    windows: Windows,
    config: Res<Config>,
    mut commands: Commands,
) {
    let get_window_frame = |entity| get_moving_window_frame(entity, &windows);

    for entity in marker {
        debug!("reshuffle_layout_strip: triggered for entity {entity}");
        if let Ok(mut cmd) = commands.get_entity(entity) {
            cmd.try_remove::<ReshuffleAroundMarker>();
        }

        let Some(frame) = expose_window(entity, &windows, &active_display, &config) else {
            return;
        };

        let layout_strip = active_display.active_strip();

        let Some((_, abs_position)) = layout_strip.index_of(entity).ok().and_then(|index| {
            layout_strip
                .absolute_positions(&get_window_frame)
                .nth(index)
        }) else {
            continue;
        };
        let viewport_position =
            Origin::new(frame.min.x - abs_position, active_display.bounds().min.y);

        let absolute_positions = active_display
            .active_strip()
            .absolute_positions(&get_window_frame)
            .collect::<Vec<_>>();
        let viewport = active_display
            .display()
            .actual_display_bounds(active_display.dock(), &config);
        let viewport_x = if frame.center().x == viewport.center().x {
            // Attempting to cente window, don't clamp.
            viewport_position.x
        } else {
            clamp_viewport_offset(
                viewport_position.x,
                0,
                &absolute_positions,
                &get_window_frame,
                &viewport,
                &config,
            )
            .unwrap_or(viewport_position.x)
        };
        reposition_entity(
            active_display.active_strip_entity(),
            viewport_position.with_x(viewport_x),
            &mut commands,
        );
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn pump_events(
    mut exit: MessageWriter<AppExit>,
    mut messages: MessageWriter<Event>,
    incoming_events: Option<NonSend<Receiver<Event>>>,
    platform: Option<NonSendMut<Pin<Box<PlatformCallbacks>>>>,
    mut timeout: Local<u32>,
) {
    const LOOP_MAX_TIMEOUT_MS: u32 = 500;
    const LOOP_TIMEOUT_STEP: u32 = 1;

    let Some((ref mut platform, incoming_events)) = platform.zip(incoming_events) else {
        // No platform interface or incoming event pipe - probably executing in a unit test.
        return;
    };

    platform.pump_cocoa_event_loop(f64::from(*timeout) / 1000.0);
    loop {
        // Repeatedly drain the events until timeout.
        match incoming_events.recv_timeout(Duration::from_millis(1)) {
            Ok(Event::Exit) | Err(RecvTimeoutError::Disconnected) => {
                exit.write(AppExit::Success);
                break;
            }
            Ok(event) => {
                messages.write(event);
                *timeout = LOOP_TIMEOUT_STEP;
            }
            Err(RecvTimeoutError::Timeout) => {
                *timeout = timeout.min(LOOP_MAX_TIMEOUT_MS) + LOOP_TIMEOUT_STEP;
                break;
            }
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn window_update_frame(
    mut messages: MessageReader<Event>,
    mut windows: Query<(
        &mut Window,
        Entity,
        &mut Position,
        &mut Bounds,
        Has<StackAdjustedResize>,
    )>,
    focused: Option<Single<Entity, With<FocusedMarker>>>,
    active_display: ActiveDisplay,
    active_workspace: Query<&Scrolling, With<ActiveWorkspaceMarker>>,
    config: Configuration,
    mut commands: Commands,
) {
    for event in messages.read() {
        match event {
            Event::WindowMoved { .. } | Event::WindowResized { .. }
                if active_workspace
                    .iter()
                    .next()
                    .is_some_and(|marker| marker.is_user_swiping) => {}
            Event::WindowMoved { window_id } | Event::WindowResized { window_id } => {
                let (entity, old_frame, new_frame) = {
                    let Some((mut window, entity, mut position, mut bounds, stack_adjusted)) =
                        windows
                            .iter_mut()
                            .find(|window| window.0.id() == *window_id)
                    else {
                        continue;
                    };
                    let Ok(new_frame) = window.update_frame() else {
                        continue;
                    };

                    // Skip reshuffle for resize events that we caused ourselves when
                    // adjusting an adjacent stacked window's height (see below).
                    if stack_adjusted {
                        commands.entity(entity).try_remove::<StackAdjustedResize>();
                        continue;
                    }

                    if active_display.active_strip().index_of(entity).is_err() {
                        // Do not reshuffle for floating windows or on other displays or
                        // workspaces.
                        continue;
                    }

                    let old_frame = IRect::from_corners(position.0, position.0 + bounds.0);
                    if matches!(event, Event::WindowMoved { window_id: _ })
                        || old_frame.min != new_frame.min
                    // Resized from the left, so the origin got moved.
                    {
                        position.0 = new_frame.min;
                    }
                    if matches!(event, Event::WindowResized { window_id: _ }) {
                        bounds.0 = new_frame.size();
                    }
                    (entity, old_frame, new_frame)
                };

                if matches!(event, Event::WindowResized { window_id: _ }) && !config.initializing()
                {
                    // When the user drags the top edge of a stacked window, macOS
                    // changes both its origin.y and height while leaving its bottom
                    // edge unchanged.  The window above hasn't been resized, so its
                    // stored height + this window's new height > viewport, causing
                    // binpack to fight the drag.  Fix: resize the window above so
                    // that A.height = gap between their origins.
                    let is_top_edge_drag = old_frame.min.y != new_frame.min.y
                        && old_frame.max.y.abs_diff(new_frame.max.y) <= 2;

                    if is_top_edge_drag
                        && let Some(above_entity) = active_display.active_strip().above(entity)
                    {
                        if let Ok((_, _, above_pos, mut bounds, _)) = windows.get_mut(above_entity)
                        {
                            let new_height = new_frame.min.y - above_pos.0.y;
                            if new_height > 0 {
                                bounds.0.y = new_height;
                            }
                        }
                        commands
                            .entity(above_entity)
                            .try_insert(StackAdjustedResize);
                    }

                    // Reshuffle around the focused window, not the resized one.
                    // Reshuffling around an off-screen sliver would call
                    // expose_window on it, pulling it into view and causing a
                    // feedback loop.
                    if let Some(focused) = &focused {
                        reshuffle_around(**focused, &mut commands);
                    }
                }
            }
            _ => (),
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn displays_rearranged(
    mut messages: MessageReader<Event>,
    workspaces: Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    mut displays: Query<(&mut Display, Entity)>,
    window_manager: Res<WindowManager>,
    windows: Windows,
    config: Res<Config>,
    mut commands: Commands,
) {
    for event in messages.read() {
        match event {
            Event::DisplayAdded { display_id } => {
                add_display(*display_id, &window_manager, &config, &mut commands);
            }
            Event::DisplayRemoved { display_id } => {
                remove_display(*display_id, &workspaces, &mut displays, &mut commands);
            }
            Event::DisplayMoved { display_id } => {
                move_display(
                    *display_id,
                    &mut displays,
                    &window_manager,
                    &workspaces,
                    &windows,
                    &config,
                    &mut commands,
                );
            }
            _ => continue,
        }
        commands.trigger(WMEventTrigger(Event::DisplayChanged));
    }
}

fn add_display(
    display_id: CGDirectDisplayID,
    window_manager: &Res<WindowManager>,
    config: &Config,
    commands: &mut Commands,
) {
    debug!("Display Added: {display_id:?}");
    let Some((mut display, workspaces)) = window_manager
        .0
        .present_displays()
        .into_iter()
        .find(|(display, _)| display.id() == display_id)
    else {
        error!("Unable to find added display id {display_id}!");
        return;
    };

    display.set_menubar_height_override(config.menubar_height());
    let origin = Position(display.bounds().min);
    let display_entity = commands.spawn(display).id();

    // Do not try to look for orphaned workspaces here,
    // because they may still be attached to their display.
    for id in workspaces {
        commands.spawn((
            origin.clone(),
            LayoutStrip::new(id),
            ChildOf(display_entity),
        ));
    }
}

fn remove_display(
    display_id: CGDirectDisplayID,
    workspaces: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    displays: &mut Query<(&mut Display, Entity)>,
    commands: &mut Commands,
) {
    const ORPHANED_SPACES_TIMEOUT_SEC: u64 = 30;
    debug!("Display Removed: {display_id:?}");
    let Some((display, display_entity)) = displays
        .into_iter()
        .find(|(display, _)| display.id() == display_id)
    else {
        error!("Unable to find removed display!");
        return;
    };

    for (strip, entity, _) in workspaces
        .into_iter()
        .filter(|(_, _, child)| child.is_some_and(|child| child.parent() == display_entity))
    {
        if strip.len() == 0 {
            // There are no windows on the layout strip, don't bother orphaning them.
            continue;
        }
        let display_id = display.id();
        debug!(
            "orphaning strip {} after removal of display {display_id}.",
            strip.id(),
        );
        let timeout = Timeout::new(
            Duration::from_secs(ORPHANED_SPACES_TIMEOUT_SEC),
            Some(format!(
                "Orphaned strip {} ({strip}) could not be re-inserted after {ORPHANED_SPACES_TIMEOUT_SEC}s.",
                strip.id()
            )),
        );
        if let Ok(mut commands) = commands.get_entity(entity) {
            commands.try_insert(timeout);
        }
        if let Ok(mut commands) = commands.get_entity(display_entity) {
            commands.detach_child(entity);
        }
    }

    if let Ok(mut commands) = commands.get_entity(display_entity) {
        commands.despawn();
    }
}

fn move_display(
    display_id: CGDirectDisplayID,
    displays: &mut Query<(&mut Display, Entity)>,
    window_manager: &Res<WindowManager>,
    existing_strips: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    windows: &Windows,
    config: &Config,
    commands: &mut Commands,
) {
    debug!("Display Moved: {display_id:?}");
    let Some((mut display, display_entity)) = displays
        .iter_mut()
        .find(|(display, _)| display.id() == display_id)
    else {
        error!("Unable to find moved display!");
        return;
    };
    let Some((moved_display, workspaces)) = window_manager
        .0
        .present_displays()
        .into_iter()
        .find(|(display, _)| display.id() == display_id)
    else {
        return;
    };
    *display = moved_display;
    display.set_menubar_height_override(config.menubar_height());

    let moved_workspaces = existing_strips
        .iter()
        .filter(|(workspace, _, _)| workspaces.iter().any(|id| workspace.id() == *id));
    for (strip, entity, _) in moved_workspaces {
        if let Ok(mut cmd) = commands.get_entity(entity) {
            cmd.try_remove::<Timeout>()
                .try_remove::<ChildOf>()
                .insert(ChildOf(display_entity));
        }
        refresh_workspace_window_sizes(
            strip.id(),
            &strip.all_windows(),
            windows,
            &display,
            window_manager,
            commands,
        );
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn gather_initial_processes(
    receiver: Option<NonSendMut<Receiver<Event>>>,
    mut displays: Query<&mut Display>,
    mut commands: Commands,
) {
    let Some(receiver) = receiver else {
        // Probably running in a mock environment, ignore.
        return;
    };
    let mut initial_processes: Vec<BProcess> = Vec::new();
    let mut initial_config = None;
    loop {
        match receiver.recv().expect("error reading initial processes") {
            Event::ProcessesLoaded | Event::Exit => break,
            Event::ApplicationLaunched { psn, observer } => {
                initial_processes.push(Process::new(&psn, observer.clone()).into());
            }
            Event::InitialConfig(config) => {
                // If there is a display menubar override, apply it to newly created displays.
                let height = config.menubar_height();
                for mut display in &mut displays {
                    display.set_menubar_height_override(height);
                }

                initial_config = Some(config);
            }
            event => warn!("Stray event during initial process gathering: {event:?}"),
        }
    }
    if let Some(config) = initial_config {
        commands.insert_resource(config);
    }

    while let Some(mut process) = initial_processes.pop() {
        if process.is_observable() {
            debug!("Adding existing process {}", process.name());
            commands.spawn((ExistingMarker, process));
        } else {
            debug!(
                "Existing application '{}' is not observable, ignoring it.",
                process.name(),
            );
        }
    }
}

#[instrument(level = Level::TRACE, skip_all, fields(entity), ret)]
fn get_moving_window_frame(entity: Entity, windows: &Windows) -> Option<IRect> {
    windows
        .positioning(entity)
        .map(|(origin, size, _, reposition, resize)| {
            let size = size.0;
            let mut frame = IRect::from_corners(origin.0, origin.0 + size);

            if let Some(reposition) = reposition {
                frame.min = reposition.0;
                frame.max = frame.min + size;
            }
            if let Some(resize) = resize {
                frame.max = frame.min + resize.0;
            }
            frame
        })
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn position_layout_strip(
    moved_strips: Populated<(&mut LayoutStrip, &Position, &ChildOf), Changed<Position>>,
    mut windows: Query<(&Window, Entity, &mut Position, &mut Bounds), Without<LayoutStrip>>,
    position: Query<(Option<&RepositionMarker>, Option<&ResizeMarker>), With<Window>>,
    active_display: Single<(&Display, Entity, Option<&DockPosition>), With<ActiveDisplayMarker>>,
    active_workspace: Query<&Scrolling, With<ActiveWorkspaceMarker>>,
    config: Res<Config>,
) {
    let (active_display, display_entity, dock) = *active_display;
    let viewport = active_display.actual_display_bounds(dock, &config);
    let offscreen_sliver_width = config.sliver_width();
    let (_, pad_right, _, pad_left) = config.edge_padding();

    let get_window_frame = |entity| {
        let mut frame = windows
            .get(entity)
            .map(|(_, _, position, bounds)| IRect::from_corners(position.0, position.0 + bounds.0))
            .ok()?;
        let (moving, sizing) = position.get(entity).ok()?;
        if let Some(moving) = moving {
            frame.min = moving.0;
        }
        if let Some(sizing) = sizing {
            frame.max = frame.min + sizing.0;
        }
        Some(frame)
    };

    let mut resized = Vec::new();
    let mut moved = Vec::new();
    for (layout_strip, position, child) in moved_strips {
        if child.parent() != display_entity {
            // Display was changed, but the workspace has not changed yet. Or vice-versa.
            continue;
        }

        for (entity, mut frame) in
            layout_strip.layout_to_viewport(**position, &viewport, &get_window_frame)
        {
            let Some(old_frame) = get_window_frame(entity) else {
                continue;
            };
            // Account for per-window horizontal_padding: reposition() adds
            // h_pad to the virtual x, so subtract it here so the OS window
            // lands exactly sliver_width pixels from the screen edge.
            let h_pad = windows
                .get(entity)
                .map(|w| w.0.horizontal_padding())
                .unwrap_or(0);

            let width = frame.width();
            if frame.max.x <= viewport.min.x {
                // Window hidden to the left
                frame.min.x += offscreen_sliver_width.max(pad_left) - pad_left;
                frame.min.x += h_pad;
            } else if frame.min.x >= viewport.max.x {
                // Window hidden to the right
                frame.min.x -= offscreen_sliver_width.max(pad_right) - pad_right;
                frame.min.x -= h_pad;
            }
            frame.max.x = frame.min.x + width;

            // During swipe, keep full height.
            let swiping = active_workspace
                .iter()
                .next()
                .is_some_and(|scrolling| scrolling.is_user_swiping);
            if !swiping {
                let stacked = layout_strip
                    .index_of(entity)
                    .ok()
                    .and_then(|idx| layout_strip.get(idx).ok())
                    .is_some_and(|col| matches!(col, Column::Stack(_)));

                // Don't compress stacked windows vertically when off-screen.
                // The height reduction corrupts their proportions: when the
                // column scrolls back on-screen, binpack_heights makes the
                // last window absorb all remaining space.
                if !stacked {
                    let inset = (f64::from(viewport.height()) * (1.0 - config.sliver_height())
                        / 2.0) as i32;
                    frame.min.y += inset;
                    frame.max.y += inset;
                }
            }

            if old_frame.size() != frame.size() {
                resized.push((entity, Size::new(frame.width(), frame.height())));
            }

            if old_frame.min != frame.min {
                moved.push((entity, frame.min));
            }
        }
    }

    for (entity, origin) in moved {
        if let Ok((_, _, mut position, _)) = windows.get_mut(entity) {
            position.0 = origin;
        }
    }
    for (entity, size) in resized {
        if let Ok((_, _, _, mut bounds)) = windows.get_mut(entity) {
            bounds.0 = size;
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn reposition_dragged_window(
    markers: Populated<(&Timeout, &WindowDraggedMarker, Entity)>,
    active_workspace: Query<&Scrolling, With<ActiveWorkspaceMarker>>,
    mut commands: Commands,
) {
    // After a swipe, stale drag markers would cause reshuffle_layout_strip
    // to snap the viewport home (expose_window bumps off-screen entities
    // to the display edge, resetting viewport_offset ≈ 0).  Grace period
    // covers the 1s drag-marker timeout.
    if active_workspace
        .iter()
        .next()
        .is_some_and(|marker| marker.is_user_swiping)
    {
        for (_, _, marker_entity) in &markers {
            commands.entity(marker_entity).despawn();
        }
        return;
    }

    for (
        timeout,
        WindowDraggedMarker {
            entity,
            display_id: _,
        },
        _,
    ) in markers
    {
        if timeout.timer.is_finished() {
            debug!("Window {entity} dragged, refreshing layout.");
            reshuffle_around(*entity, &mut commands);
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn update_overlays(
    windows: Windows,
    applications: Query<&Application>,
    _: ActiveDisplay, // prevents this system from running without an active workspace
    active_workspace: Query<&Scrolling, With<ActiveWorkspaceMarker>>,
    overlay_mgr: Option<NonSendMut<OverlayManager>>,
    config: Configuration,
) {
    use crate::overlay::BorderParams;
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let Some(mut overlay_mgr) = overlay_mgr else {
        return;
    };

    let dim_opacity = config.config().dim_inactive_opacity();
    let border_enabled = config.config().border_active_window();

    // Hide overlays during swipe, mission control, native fullscreen spaces,
    // or briefly after a space change (macOS space-switch animation).
    let swiping = active_workspace
        .iter()
        .next()
        .is_some_and(|marker| marker.is_user_swiping);
    // ON_FULLSCREEN_SPACE is set in workspace_change_trigger because this
    // system cannot run when no LayoutStrip has ActiveWorkspaceMarker
    // (which is the case on native fullscreen spaces).
    if swiping || config.mission_control_active() {
        overlay_mgr.hide_all();
        return;
    }

    if dim_opacity == 0.0 && !border_enabled {
        overlay_mgr.remove_all();
        return;
    }

    // Find the focused managed window's absolute CG frame.
    // Skip floating/unmanaged windows — no overlay or border for those.
    let (focused_abs_cg, focused_border_radius, detected_border_radius) =
        if let Some((window, _, unmanaged)) = windows
            .focused()
            .and_then(|(_, entity)| windows.get_managed(entity))
            && unmanaged.is_none()
            && !window.is_full_screen()
        {
            let frame = window.frame();
            let h_pad = window.horizontal_padding();
            let v_pad = window.vertical_padding();
            let focused_abs_cg = Some(NSRect::new(
                NSPoint::new(
                    f64::from(frame.min.x + h_pad),
                    f64::from(frame.min.y + v_pad),
                ),
                NSSize::new(
                    f64::from(frame.width() - 2 * h_pad),
                    f64::from(frame.height() - 2 * v_pad),
                ),
            ));

            // Look up per-window border_radius from config (dynamic, respects hot-reload).
            let title = window.title().unwrap_or_default();
            let bundle_id = windows
                .find_parent(window.id())
                .and_then(|(_, _, parent)| applications.get(parent).ok())
                .map(|app| app.bundle_id().unwrap_or_default())
                .unwrap_or_default();
            let properties = config.find_window_properties(&title, bundle_id);
            let focused_border_radius = properties.iter().find_map(|p| p.border_radius);

            (
                focused_abs_cg,
                focused_border_radius,
                window.border_radius(),
            )
        } else {
            // No managed window has focus — hide the overlay rather than
            // dimming everything (e.g. during startup or when only floating
            // windows exist).
            overlay_mgr.hide_all();
            return;
        };

    let calculated_radius = match config.config().border_radius() {
        BorderRadiusOption::Auto => detected_border_radius.unwrap_or(10.0),
        BorderRadiusOption::Value(value) => value.max(0.0),
    };

    let border_params = border_enabled.then(|| BorderParams {
        color: config.config().border_color(),
        opacity: config.config().border_opacity(),
        width: config.config().border_width(),
        radius: focused_border_radius.unwrap_or(calculated_radius),
    });

    let dim_color = config.config().dim_inactive_color();
    overlay_mgr.update(
        dim_opacity,
        dim_color,
        focused_abs_cg,
        border_params.as_ref(),
    );
}

#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn commit_window_position(
    mut moved_windows: Populated<(&mut Window, &Position), Changed<Position>>,
) {
    moved_windows
        .par_iter_mut()
        .for_each(|(mut window, position)| window.reposition(position.0));
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn commit_window_size(
    active_display: ActiveDisplay,
    mut resized_windows: Populated<(&mut Window, &Bounds, &mut WidthRatio), Changed<Bounds>>,
) {
    let display_bounds = active_display.bounds();
    resized_windows
        .par_iter_mut()
        .for_each(|(mut window, size, mut width_ratio)| {
            width_ratio.0 = f64::from(size.0.x) / f64::from(display_bounds.width());
            window.resize(size.0);
        });
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn cleanup_on_exit(
    mut exit_events: MessageReader<AppExit>,
    windows: Windows,
    window_manager: Res<WindowManager>,
) {
    for _ in exit_events.read() {
        let ids = windows
            .iter()
            .map(|(window, _)| window.id())
            .collect::<Vec<_>>();
        window_manager.dim_windows(&ids, 0.0);
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn swipe_gesture(
    mut messages: MessageReader<Event>,
    active_display: ActiveDisplay,
    mut active_workspace: Single<(Entity, Option<&mut Scrolling>), With<ActiveWorkspaceMarker>>,
    time: Res<Time>,
    config: Configuration,
    mut commands: Commands,
) {
    if config.mission_control_active() {
        return;
    }

    for event in messages.read() {
        let Event::Swipe { deltas } = event else {
            continue;
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

        let dt = time.delta_secs_f64();
        let new_velocity = if dt > 0.0 {
            delta * config.config().swipe_sensitivity() / dt
        } else {
            0.0
        };

        let (entity, scrolling) = &mut *active_workspace;
        if let Some(scrolling) = scrolling.as_mut() {
            let velocity = 0.3 * new_velocity + 0.7 * scrolling.velocity;
            scrolling.velocity = velocity;
            scrolling.is_user_swiping = true;
            scrolling.last_event = Instant::now();
        } else if let Ok(mut entity_cmmands) = commands.get_entity(*entity) {
            entity_cmmands.try_insert(Scrolling {
                velocity: new_velocity,
                is_user_swiping: true,
                ..Default::default()
            });
        }
    }
}

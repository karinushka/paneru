use bevy::app::AppExit;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::{ChildOf, Children};
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::query::{Has, Or, With, Without};
use bevy::ecs::system::{Commands, Local, NonSend, NonSendMut, Populated, Query, Res, Single};
use bevy::math::IRect;
use bevy::tasks::AsyncComputeTaskPool;
use bevy::tasks::futures_lite::future;
use bevy::time::Time;
use objc2_core_graphics::CGDirectDisplayID;
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;
use tracing::{Level, debug, error, info, instrument, trace, warn};

use super::{
    ActiveDisplayMarker, BProcess, ExistingMarker, FocusedMarker, FreshMarker,
    PollForNotifications, RepositionMarker, ResizeMarker, SpawnWindowTrigger, Timeout,
    WMEventTrigger,
};
use crate::config::Config;
use crate::ecs::params::{ActiveDisplay, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, BruteforceWindows, DockPosition, Initializing, LocateDockTrigger,
    ReshuffleAroundMarker, Unmanaged, WindowDraggedMarker, WindowSwipeMarker, reposition_entity,
    reshuffle_around, resize_entity,
};
use crate::events::Event;
use crate::manager::{
    Application, Column, Display, LayoutStrip, Origin, Process, Size, Window, WindowManager,
    WindowOS, bruteforce_windows,
};
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
                commands.spawn((strip, ActiveWorkspaceMarker, ChildOf(entity)));
            } else {
                commands.spawn((strip, ChildOf(entity)));
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

        if active_strip
            && let Some(window) = strip
                .first()
                .ok()
                .and_then(|column| column.top())
                .and_then(|entity| windows.get(entity))
            && let Some(psn) = windows.psn(window.id(), &apps)
        {
            debug!("raising {}", window.id());
            window.focus_with_raise(psn);
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
    app_query: Populated<(&mut Application, Entity), With<FreshMarker>>,
    windows: Windows,
    mut commands: Commands,
) {
    // TODO: maybe refactor this with add_existing_application_windows()
    let find_window = |window_id| windows.find(window_id);

    for (app, entity) in app_query {
        let mut create_windows = app.window_list();
        // Retain the non-existing windows, so they can be created.
        create_windows.retain(|window| find_window(window.id()).is_none());

        if !create_windows.is_empty() {
            commands.entity(entity).try_remove::<FreshMarker>();
            debug!("spawn!");
            commands.trigger(SpawnWindowTrigger(create_windows));
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

/// A Bevy system that finds and re-assigns orphaned spaces to the active display.
/// This system iterates through `OrphanedStrip` entities, attempts to merge their windows into an existing space on the active display,
/// and then despawns the `OrphanedStrip` entity.
///
/// # Arguments
///
/// * `orphaned_spaces` - A `Populated` query for `(Entity, &mut OrphanedStrip)` components.
/// * `active_display` - A mutable `ActiveDisplayMut` system parameter for the currently active display.
/// * `commands` - Bevy commands to despawn entities.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn find_orphaned_workspaces(
    orphans: Populated<(&LayoutStrip, Entity), Without<ChildOf>>,
    workspaces: Populated<(&LayoutStrip, Entity, &ChildOf), With<ChildOf>>,
    windows: Windows,
    displays: Query<&Display>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let matched_orphans = workspaces.into_iter().filter_map(|(strip, entity, child)| {
        orphans.iter().find_map(|(orphan, orphan_entity)| {
            (strip.id() == orphan.id()).then_some((
                child.parent(),
                strip,
                entity,
                orphan,
                orphan_entity,
            ))
        })
    });

    for (parent_display, strip, entity, orphan, orphan_entity) in matched_orphans {
        let Ok(display) = displays.get(parent_display) else {
            continue;
        };
        let display_id = display.id();
        debug!(
            "Re-inserting orphaned strip: {parent_display}, {}, {entity}, {}, {orphan_entity}, display {display_id}",
            strip.id(),
            orphan.id(),
        );

        if let Ok(mut commands) = commands.get_entity(orphan_entity) {
            commands.try_remove::<Timeout>();
        }
        if let Ok(mut commands) = commands.get_entity(orphan_entity) {
            commands.try_insert(ChildOf(parent_display));
        }
        if let Ok(mut commands) = commands.get_entity(entity) {
            commands.try_despawn();
        }

        let mut in_workspace = window_manager
            .windows_in_workspace(strip.id())
            .inspect_err(|err| {
                warn!("getting windows in workspace: {err}");
            })
            .unwrap_or_default();

        for entity in orphan.all_windows() {
            // Update window ratios on the new display.
            if let Some(window) = windows.get(entity) {
                let width = f64::from(display.width()) * window.width_ratio();
                let height = display.height();
                debug!(
                    "refreshing ratio {:.1} for window {}: {:.0}x{:.0}",
                    window.width_ratio(),
                    window.id(),
                    width,
                    height,
                );
                resize_entity(
                    entity,
                    Size::new(width as i32, height),
                    display.id(),
                    &mut commands,
                );

                in_workspace.retain(|window_id| *window_id != window.id());
            }
        }

        // Find remaining windows which are outside of the strip.
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
            reposition_entity(
                window_entity,
                display.absolute_coords(Origin::default()),
                display.id(),
                &mut commands,
            );
        }
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
pub(super) fn animate_windows(
    windows: Populated<(&mut Window, Entity, &RepositionMarker)>,
    displays: Query<&Display>,
    time: Res<Time>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let move_ratio = config.animation_speed() * time.delta_secs_f64();

    for (mut window, entity, RepositionMarker { origin, display_id }) in windows {
        let Some(display) = displays.iter().find(|display| display.id() == *display_id) else {
            continue;
        };
        let move_delta = move_ratio * f64::from(display.width());
        let delta = window
            .frame()
            .min
            .as_vec2()
            .move_towards(origin.as_vec2(), move_delta as f32)
            .as_ivec2();

        trace!(
            "window {} source {} dest {origin} delta {move_delta} moving to {delta}",
            window.id(),
            window.frame().min,
        );
        window.reposition(delta);
        if *origin == delta {
            commands.entity(entity).try_remove::<RepositionMarker>();
        }
    }
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
pub(super) fn animate_resize_windows(
    windows: Populated<(&mut Window, Entity, &ResizeMarker, Has<RepositionMarker>)>,
    displays: Query<&Display>,
    time: Res<Time>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let move_ratio = config.animation_speed() * time.delta_secs_f64();

    for (mut window, entity, ResizeMarker { size, display_id }, moving) in windows {
        if moving {
            // Wait resizing a window which is moving.
            continue;
        }
        let Some(display) = displays.iter().find(|display| display.id() == *display_id) else {
            continue;
        };
        let move_delta = move_ratio * f64::from(display.width());
        let origin = window.frame().size();
        let delta = origin
            .as_vec2()
            .move_towards(size.as_vec2(), move_delta as f32)
            .as_ivec2();

        trace!(
            "window {} source {origin} dest {size} delta {move_delta} resizing to {delta}",
            window.id(),
        );
        window.resize(delta, display.width());
        if *size == delta {
            commands.entity(entity).try_remove::<ResizeMarker>();
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn window_swiper(
    sliding: Populated<(Entity, Has<Unmanaged>, &WindowSwipeMarker)>,
    windows: Query<(&Window, Option<&RepositionMarker>, Option<&ResizeMarker>)>,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
) {
    let get_window_frame = |entity| get_moving_window_frame(entity, &active_display, &windows);
    let get_window_h_pad = |entity: Entity| {
        windows
            .get(entity)
            .map(|(w, _, _)| w.horizontal_padding())
            .unwrap_or(0)
    };
    let mut viewport = active_display.bounds();
    viewport.max.y = viewport.min.y + get_display_height(&active_display);

    for (entity, unmanaged, WindowSwipeMarker(delta)) in sliding {
        let shift = (f64::from(viewport.width()) * delta) as i32;

        commands.entity(entity).try_remove::<WindowSwipeMarker>();

        if unmanaged && let Some(frame) = get_window_frame(entity) {
            // Window is floating, just shift it directly.
            let mut origin = frame.min;
            origin.x -= shift;
            reposition_entity(entity, origin, active_display.id(), &mut commands);
            continue;
        }

        let strip = active_display.active_strip();
        let mut absolute_position = None;
        let mut total_strip_width = 0_i32;
        for (column, pos) in strip.absolute_positions(&get_window_frame) {
            if column.top().is_some_and(|col| col == entity) {
                absolute_position = Some(pos);
            }
            if let Some(top) = column.top()
                && let Some(frame) = get_window_frame(top)
            {
                total_strip_width = pos + frame.width();
            }
        }
        let Some(viewport_offset) = absolute_position
            .zip(get_window_frame(entity))
            .map(|(pos, frame)| pos - (frame.min.x - viewport.min.x))
        else {
            continue;
        };

        let clamped_offset = if config.free_slide() {
            viewport_offset + shift
        } else {
            let (_, pad_right, _, pad_left) = config.edge_padding();
            let effective_width = viewport.width() - pad_left - pad_right;
            let left_aligned = -pad_left;
            let right_aligned = total_strip_width - effective_width - pad_left;
            (viewport_offset + shift).clamp(left_aligned, right_aligned.max(left_aligned))
        };

        position_layout_windows(
            clamped_offset,
            &active_display,
            &get_window_frame,
            &get_window_h_pad,
            &config,
            &mut commands,
        );
    }
}

fn expose_window(
    entity: Entity,
    windows: &Query<(&Window, Option<&RepositionMarker>, Option<&ResizeMarker>)>,
    active_display: &ActiveDisplay,
    dock: Option<&DockPosition>,
    edge_padding: (i32, i32, i32, i32),
) -> Option<IRect> {
    let (_, pad_right, _, pad_left) = edge_padding;
    let display_bounds = active_display.bounds();
    let mut frame = get_moving_window_frame(entity, active_display, windows)?;
    let size = frame.size();

    if frame.max.x > display_bounds.max.x - pad_right {
        trace!("Bumped window {entity} to the left");
        frame.min.x = display_bounds.max.x - pad_right - size.x;
    } else if frame.min.x < display_bounds.min.x + pad_left {
        trace!("Bumped window {entity} to the right");
        frame.min.x = display_bounds.min.x + pad_left;
    }

    if let Some(dock) = dock {
        match dock {
            DockPosition::Left(offset) => {
                if frame.min.x < display_bounds.min.x + *offset {
                    frame.min.x = display_bounds.min.x + *offset;
                }
            }
            DockPosition::Right(offset) => {
                if frame.min.x + size.x > display_bounds.max.x - *offset {
                    frame.min.x = display_bounds.min.x - size.x - *offset;
                }
            }
            _ => (),
        }
    }
    frame.max.x = frame.min.x + size.x;
    Some(frame)
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn reshuffle_layout_strip(
    marker: Populated<Entity, With<ReshuffleAroundMarker>>,
    active_display: ActiveDisplay,
    windows: Query<(&Window, Option<&RepositionMarker>, Option<&ResizeMarker>)>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let get_window_frame = |entity| get_moving_window_frame(entity, &active_display, &windows);
    let get_window_h_pad = |entity: Entity| {
        windows
            .get(entity)
            .map(|(w, _, _)| w.horizontal_padding())
            .unwrap_or(0)
    };

    for entity in marker {
        if let Ok(mut cmd) = commands.get_entity(entity) {
            cmd.try_remove::<ReshuffleAroundMarker>();
        }

        let Some(frame) = expose_window(
            entity,
            &windows,
            &active_display,
            active_display.dock(),
            config.edge_padding(),
        ) else {
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
        let viewport_offset = abs_position - (frame.min.x - active_display.bounds().min.x);

        position_layout_windows(
            viewport_offset,
            &active_display,
            &get_window_frame,
            &get_window_h_pad,
            &config,
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
    mut windows: Query<(&mut Window, Entity)>,
    focused: Option<Single<Entity, With<FocusedMarker>>>,
    active_display: ActiveDisplay,
    mut commands: Commands,
) {
    for event in messages.read() {
        match event {
            Event::WindowMoved { window_id } | Event::WindowResized { window_id } => {
                if let Some((mut window, entity)) = windows
                    .iter_mut()
                    .find(|(window, _)| window.id() == *window_id)
                {
                    _ = window.update_frame(&active_display.bounds());

                    if active_display.active_strip().index_of(entity).is_err() {
                        // Do not reshuffle for floating windows or on other displays or
                        // workspaces.
                        continue;
                    }

                    if matches!(event, Event::WindowResized { window_id: _ }) {
                        // Reshuffle around the focused window, not the resized one.
                        // Reshuffling around an off-screen sliver would call
                        // expose_window on it, pulling it into view and causing a
                        // feedback loop.
                        let target = focused.as_deref().copied().unwrap_or(entity);
                        reshuffle_around(target, &mut commands);
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
    workspaces: Query<(&LayoutStrip, Entity, &ChildOf)>,
    mut displays: Query<(&mut Display, Entity)>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    for event in messages.read() {
        match event {
            Event::DisplayAdded { display_id } => {
                add_display(*display_id, &window_manager, &mut commands);
            }
            Event::DisplayRemoved { display_id } => {
                remove_display(*display_id, &workspaces, &mut displays, &mut commands);
            }
            Event::DisplayMoved { display_id } => {
                move_display(*display_id, &mut displays, &window_manager);
            }
            _ => continue,
        }
        commands.trigger(WMEventTrigger(Event::DisplayChanged));
    }
}

fn add_display(
    display_id: CGDirectDisplayID,
    window_manager: &Res<WindowManager>,
    commands: &mut Commands,
) {
    debug!("Display Added: {display_id:?}");
    let Some((display, workspaces)) = window_manager
        .0
        .present_displays()
        .into_iter()
        .find(|(display, _)| display.id() == display_id)
    else {
        error!("Unable to find added display id {display_id}!");
        return;
    };
    // find_orphaned_spaces(&mut orphaned_spaces.0, &mut display, &mut windows);

    let children = workspaces
        .into_iter()
        .map(|id| commands.spawn(LayoutStrip::new(id)).id())
        .collect::<Vec<_>>();
    commands.spawn(display).add_children(&children);
}

fn remove_display(
    display_id: CGDirectDisplayID,
    workspaces: &Query<(&LayoutStrip, Entity, &ChildOf)>,
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
        .filter(|(_, _, child)| child.parent() == display_entity)
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
) {
    debug!("Display Moved: {display_id:?}");
    let Some((mut display, _)) = displays
        .iter_mut()
        .find(|(display, _)| display.id() == display_id)
    else {
        error!("Unable to find moved display!");
        return;
    };
    let Some((moved_display, _)) = window_manager
        .0
        .present_displays()
        .into_iter()
        .find(|(display, _)| display.id() == display_id)
    else {
        return;
    };
    *display = moved_display;
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn gather_initial_processes(
    receiver: Option<NonSendMut<Receiver<Event>>>,
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
fn get_moving_window_frame(
    entity: Entity,
    active_display: &ActiveDisplay,
    windows: &Query<(&Window, Option<&RepositionMarker>, Option<&ResizeMarker>)>,
) -> Option<IRect> {
    windows
        .get(entity)
        .map(|(window, reposition, resize)| {
            let mut frame = window.frame();
            let size = window.frame().size();

            if let Some(reposition) = reposition
                && reposition.display_id == active_display.id()
            {
                frame.min = reposition.origin;
                frame.max = frame.min + size;
            }
            if let Some(resize) = resize
                && resize.display_id == active_display.id()
            {
                frame.max = frame.min + resize.size;
            }
            frame
        })
        .inspect_err(|err| warn!("can not get frame of {entity}: {err}"))
        .ok()
}

fn get_display_height(active_display: &ActiveDisplay) -> i32 {
    let dock_size = active_display.dock().map_or(0, |dock| {
        if let DockPosition::Bottom(offset) = dock {
            *offset
        } else {
            0
        }
    });
    active_display.bounds().height() - dock_size
}

#[allow(clippy::cast_possible_truncation)]
fn position_layout_windows<W, P>(
    viewport_offset: i32,
    active_display: &ActiveDisplay,
    get_window_frame: &W,
    get_window_h_pad: &P,
    config: &Config,
    commands: &mut Commands,
) where
    W: Fn(Entity) -> Option<IRect>,
    P: Fn(Entity) -> i32,
{
    let menubar_height = active_display.display().menubar_height();
    let bounds = IRect::new(
        0,
        0,
        active_display.bounds().width(),
        get_display_height(active_display),
    );

    // Shrink the layout viewport by global edge padding.
    // calculate_layout only uses viewport.size (not origin), so we apply
    // the origin offset to each frame after layout. We also adjust
    // viewport_offset by pad_left because the caller computes it from
    // current window positions (which already include pad_left from a
    // previous layout pass).
    let (pad_top, pad_right, pad_bottom, pad_left) = config.edge_padding();
    let mut bounds = bounds;
    bounds.max.x -= pad_left + pad_right;
    bounds.max.y -= pad_top + pad_bottom;
    let viewport_offset = viewport_offset + pad_left;

    let display_width = active_display.bounds().width();
    let other_display = active_display.other().next();
    let display_above = other_display
        .is_some_and(|other_display| active_display.bounds().min.y > other_display.bounds().min.y);

    let padded_right = display_width - pad_right;
    let layout_strip = active_display.active_strip();
    for (entity, frame) in
        layout_strip.calculate_layout(viewport_offset, &bounds, &get_window_frame)
    {
        let Some(old_frame) = get_window_frame(entity) else {
            continue;
        };
        let mut frame = IRect::from_corners(
            active_display.display().absolute_coords(frame.min),
            active_display.display().absolute_coords(frame.max),
        );

        // Apply horizontal edge padding offset. calculate_layout returns
        // positions relative to 0, so shift visible windows by pad_left.
        frame.min.x += pad_left;
        frame.max.x += pad_left;

        // A window is a "sliver" if it has very little visible area within
        // the padded viewport. Only push truly off-screen windows to show
        // config.sliver_width() pixels from the screen edge.
        let sliver_width = config.sliver_width();
        let visible_left = frame.min.x.max(pad_left);
        let visible_right = frame.max.x.min(padded_right);
        let visible = (visible_right - visible_left).max(0);
        let is_off_screen = visible <= 20;

        if is_off_screen {
            // Account for per-window horizontal_padding: reposition() adds
            // h_pad to the virtual x, so subtract it here so the OS window
            // lands exactly sliver_width pixels from the screen edge.
            let h_pad = get_window_h_pad(entity);
            let width = frame.width();
            let window_center = frame.min.x + width / 2;
            if window_center <= pad_left {
                frame.min.x = sliver_width + h_pad - width;
                frame.max.x = sliver_width + h_pad;
            } else {
                frame.min.x = display_width - sliver_width - h_pad;
                frame.max.x = frame.min.x + width;
            }

            let is_stacked = layout_strip
                .index_of(entity)
                .ok()
                .and_then(|idx| layout_strip.get(idx).ok())
                .is_some_and(|col| matches!(col, Column::Stack(_)));

            if is_stacked {
                // Don't compress stacked windows vertically when off-screen.
                // The height reduction corrupts their proportions: when the
                // column scrolls back on-screen, binpack_heights makes the
                // last window absorb all remaining space.
                frame.min.y += menubar_height + pad_top;
                frame.max.y += menubar_height + pad_top;
            } else {
                let inset =
                    (f64::from(bounds.height()) * (1.0 - config.sliver_height()) / 2.0) as i32;
                frame.min.y += menubar_height + pad_top + inset;
                frame.max.y -= inset;
            }

            // Multi-display: nudge off-screen windows down to prevent macOS
            // from relocating them to the other display.
            if display_above {
                let bump = bounds.height() / 4;
                frame.min.y += bump;
                frame.max.y += bump;
            }
        } else {
            frame.min.y += menubar_height + pad_top;
            frame.max.y += menubar_height + pad_top;
        }

        if old_frame.size() != frame.size() {
            resize_entity(
                entity,
                Size::new(frame.width(), frame.height()),
                active_display.id(),
                commands,
            );
        }

        if old_frame.min != frame.min {
            reposition_entity(entity, frame.min, active_display.id(), commands);
        }
    }
}

pub(crate) fn reposition_dragged_window(
    markers: Populated<(&Timeout, &WindowDraggedMarker)>,
    mut commands: Commands,
) {
    for (
        timeout,
        WindowDraggedMarker {
            entity,
            display_id: _,
        },
    ) in markers
    {
        if timeout.timer.is_finished() {
            debug!("Window {entity} dragged, refreshing layout.");
            reshuffle_around(*entity, &mut commands);
        }
    }
}

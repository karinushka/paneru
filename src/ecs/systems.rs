use bevy::app::AppExit;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::{ChildOf, Children};
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::query::{Changed, Has, Or, With, Without};
use bevy::ecs::system::{
    Commands, Local, NonSend, NonSendMut, ParallelCommands, Populated, Query, Res, ResMut, Single,
};
use bevy::math::IRect;
use bevy::tasks::AsyncComputeTaskPool;
use bevy::tasks::futures_lite::future;
use bevy::time::Time;
use objc2_foundation::NSPoint;
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;
use tracing::{Level, debug, error, info, instrument, trace, warn};

use super::{
    ActiveDisplayMarker, BProcess, ExistingMarker, FreshMarker, PollForNotifications,
    RepositionMarker, ResizeMarker, RetryFrontSwitch, SpawnWindowTrigger, Timeout,
};

use crate::config::{Config, decorations::BorderRadiusOption};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{ActiveDisplay, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, BruteforceWindows, FlashMessage, Initializing,
    LocateDockTrigger, LowPowerMode, MissionControlActive, Position, RestoreWindowState, Scrolling,
    SelectedVirtualMarker, SendMessageTrigger, Unmanaged, WidthRatio, WindowProperties,
    focus_entity,
};
use crate::events::Event;
use crate::manager::{
    Application, Display, Process, Window, WindowManager, WindowOS, bruteforce_windows,
};
use crate::overlay::{FlashMessageManager, OverlayManager};
use crate::platform::PlatformCallbacks;

const ANIAMTE_SNAP_THRESHOLD: f32 = 5.0;

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

            Event::SpaceChanged if broken_notifications.is_some() => {
                info!(
                    "Workspace and display notifications arriving correctly. Disabling the polling.",
                );
                commands.remove_resource::<PollForNotifications>();
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

            _ => (),
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
            let strip = LayoutStrip::new(id, 0);
            if id == active_space {
                commands.spawn((
                    strip,
                    origin.clone(),
                    ActiveWorkspaceMarker,
                    SelectedVirtualMarker,
                    ChildOf(entity),
                ));
            } else {
                commands.spawn((
                    strip,
                    origin.clone(),
                    SelectedVirtualMarker,
                    ChildOf(entity),
                ));
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
    mut bruteforce_tasks: Query<(Entity, &mut BruteforceWindows)>,
    mut workspaces: Query<(&mut LayoutStrip, Has<ActiveWorkspaceMarker>, &ChildOf)>,
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

    for (mut strip, active_strip, _) in &mut workspaces {
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
            if !strip.contains(entity) {
                strip.append(entity);
            }
        }
        debug!("space {}: after refresh {strip:?}", strip.id());

        if active_strip && let Some(entity) = strip.first().ok().and_then(|column| column.top()) {
            focus_entity(entity, true, &mut commands);
        }
    }

    commands.remove_resource::<Initializing>();
    commands.trigger(RestoreWindowState);
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
                &mut commands,
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
            if let Some(system_id) = timeout.system_id {
                commands.run_system(system_id);
                commands.unregister_system(system_id);
            }
            trace!("Removing timer {entity}");
            commands.entity(entity).despawn();
        } else {
            timeout.timer.tick(clock.delta());
        }
    }
}

/// Retries querying the focused window for applications that had a transient AX error
/// during `ApplicationFrontSwitched`. Runs each frame until success or timeout.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn retry_front_switch(
    retries: Populated<(Entity, &RetryFrontSwitch)>,
    applications: Query<&Application>,
    mut commands: Commands,
) {
    for (entity, retry) in retries.iter() {
        let Ok(app) = applications.get(retry.0) else {
            // Application entity no longer exists, clean up.
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_despawn();
            }
            continue;
        };
        if !app.is_frontmost() {
            // App is no longer frontmost — this retry is stale.
            debug!("Discarding stale front switch retry (app no longer frontmost).");
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_despawn();
            }
            continue;
        }
        if let Ok(focused_id) = app.focused_window_id() {
            debug!("Front switch retry succeeded for window {focused_id}.");
            commands.trigger(SendMessageTrigger(Event::WindowFocused {
                window_id: focused_id,
            }));
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_despawn();
            }
        }
        // Otherwise, let timeout_ticker handle expiry.
    }
}

/// Periodically checks for displays added and removed, as well as changes in the active display.
/// This system acts as a workaround for inconsistent display change notifications on some macOS versions.
/// It uses `ThrottledSystem` to limit its execution frequency.
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
        commands.trigger(SendMessageTrigger(Event::DisplayChanged));
    } else {
        debug!("new display {current_display_id} detected.");
        commands.trigger(SendMessageTrigger(Event::DisplayAdded {
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
            commands.trigger(SendMessageTrigger(Event::DisplayRemoved {
                display_id: display.id(),
            }));
        }
    });
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
    animate: Populated<(&mut Position, Entity, &RepositionMarker)>,
    time: Res<Time>,
    config: Res<Config>,
    commands: ParallelCommands,
) {
    // Frame-rate-independent exponential smoothing (ease-out).
    // `animation_speed` is the decay rate (per second); higher = snappier.
    // t = 1 - e^(-rate*dt) is the fraction of remaining distance consumed this frame.
    let rate = config.animation_speed();
    let t = (1.0 - (-rate * time.delta_secs_f64()).exp()).clamp(0.0, 1.0) as f32;

    animate
        .into_iter()
        .for_each(|(mut position, entity, RepositionMarker(origin))| {
            let target = origin.as_vec2();
            let current = position.0.as_vec2();
            let lerped = current.lerp(target, t);

            // Snap once we're within a pixel of the target (or after one effectively-
            // complete tick), so the marker is dropped promptly.
            let finished = (target - lerped).length() <= ANIAMTE_SNAP_THRESHOLD;
            let new_pos = if finished {
                *origin
            } else {
                lerped.round().as_ivec2()
            };

            trace!(
                "entity {entity} source {} dest {origin} t {t:.3} moving to {new_pos}",
                position.0,
            );
            position.0 = new_pos;
            if finished {
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
    animate: Populated<(&mut Bounds, Entity, &ResizeMarker)>,
    time: Res<Time>,
    config: Res<Config>,
    commands: ParallelCommands,
) {
    // Matches animate_entities: exponential ease-out, frame-rate independent.
    let rate = config.animation_speed();
    let t = (1.0 - (-rate * time.delta_secs_f64()).exp()).clamp(0.0, 1.0) as f32;

    animate
        .into_iter()
        .for_each(|(mut bounds, entity, ResizeMarker(size))| {
            let target = size.as_vec2();
            let current = bounds.0.as_vec2();
            let lerped = current.lerp(target, t);

            let finished = (target - lerped).length() <= ANIAMTE_SNAP_THRESHOLD;
            let new_size = if finished {
                *size
            } else {
                lerped.round().as_ivec2()
            };

            trace!(
                "entity {entity} source {} dest {size} t {t:.3} resizing to {new_size}",
                bounds.0,
            );
            bounds.0 = new_size;
            if finished {
                commands.command_scope(|mut command| {
                    command.entity(entity).try_remove::<ResizeMarker>();
                });
            }
        });
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn pump_events(
    mut exit: MessageWriter<AppExit>,
    mut messages: MessageWriter<Event>,
    low_power_mode: Option<Res<LowPowerMode>>,
    incoming_events: Option<NonSend<Receiver<Event>>>,
    platform: Option<NonSendMut<Pin<Box<PlatformCallbacks>>>>,
    mut timeout: Local<u32>,
) {
    const LOOP_MAX_TIMEOUT_LOWPOWER_MS: u32 = 500;
    const LOOP_MAX_TIMEOUT_MS: u32 = 50;
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
                let timeout_limit = if low_power_mode.is_some_and(|low_power| low_power.0) {
                    LOOP_MAX_TIMEOUT_LOWPOWER_MS
                } else {
                    LOOP_MAX_TIMEOUT_MS
                };
                *timeout = timeout.min(timeout_limit) + LOOP_TIMEOUT_STEP;
                break;
            }
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn window_resized_update_frame(
    mut messages: MessageReader<Event>,
    mut windows: Query<(&mut Window, Entity, &Position, &mut Bounds), Without<LayoutStrip>>,
    mut active_workspace: Single<(&LayoutStrip, &mut Position), With<ActiveWorkspaceMarker>>,
) {
    for event in messages.read() {
        let Event::WindowResized { window_id } = event else {
            continue;
        };

        let Some((mut window, entity, position, mut bounds)) = windows
            .iter_mut()
            .find(|window| window.0.id() == *window_id)
        else {
            continue;
        };
        let Ok(new_frame) = window.update_frame() else {
            continue;
        };

        let old_frame = IRect::from_corners(position.0, position.0 + bounds.0);
        if old_frame.size() != new_frame.size() {
            bounds.0 = new_frame.size();
        }

        // If the window was resized, shift LayoutStrip slightly to avoid moving right corner.
        let (active_strip, ref mut strip_position) = *active_workspace;
        if !active_strip.contains(entity) {
            // Floating window, don't nudge the strip.
            continue;
        }

        if old_frame.min.x != new_frame.min.x {
            let shift = (old_frame.size() - new_frame.size()).with_y(0);
            // Search marke: reposition_entity - Updating position directly to reduce jitter.
            strip_position.0.x += shift.x;
        }

        // When the user drags the top edge of a stacked window, we adjust the window above to
        // accomodate.
        let diff = old_frame.min.y - new_frame.min.y;
        if diff.abs() > 0
            && let Some(above_entity) = active_strip.above(entity)
            && let Ok((_, _, _, mut above_bounds)) = windows.get_mut(above_entity)
            && above_bounds.0.y - diff > 200
        {
            above_bounds.0.y -= diff;
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn window_moved_update_frame(
    mut messages: MessageReader<Event>,
    mut windows: Query<(&mut Window, &mut Position, &Bounds), Without<LayoutStrip>>,
) {
    for event in messages.read() {
        let Event::WindowMoved { window_id } = event else {
            continue;
        };

        let Some((mut window, mut position, bounds)) = windows
            .iter_mut()
            .find(|window| window.0.id() == *window_id)
        else {
            continue;
        };
        let Ok(new_frame) = window.update_frame() else {
            continue;
        };

        let old_frame = IRect::from_corners(position.0, position.0 + bounds.0);
        if old_frame.min != new_frame.min {
            position.0 = new_frame.min;
        }
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

#[allow(clippy::needless_pass_by_value)]
pub(super) fn update_overlays(
    windows: Windows,
    applications: Query<&Application>,
    active_workspace: Query<(Has<Scrolling>, &LayoutStrip), With<ActiveWorkspaceMarker>>,
    overlay_mgr: Option<NonSendMut<OverlayManager>>,
    mission_control_active: Res<MissionControlActive>,
    config: Res<Config>,
) {
    use crate::overlay::BorderParams;
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let Some(mut overlay_mgr) = overlay_mgr else {
        return;
    };

    let dim_opacity = config.dim_inactive_opacity();
    let border_enabled = config.border_active_window();

    // Hide overlays during swipe, mission control, native fullscreen spaces,
    // or briefly after a space change (macOS space-switch animation).
    let Some((swiping, active_strip)) = active_workspace.iter().next() else {
        return;
    };

    if swiping || mission_control_active.0 || active_strip.is_fullscreen() {
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
            let Some(app) = windows
                .find_parent(window.id())
                .and_then(|(_, _, parent)| applications.get(parent).ok())
            else {
                return;
            };
            let properties = WindowProperties::new(app, window, &config);
            let focused_border_radius = properties.border_radius();
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

    let calculated_radius = match config.border_radius() {
        BorderRadiusOption::Auto => detected_border_radius.unwrap_or(10.0),
        BorderRadiusOption::Value(value) => value.max(0.0),
    };

    let border_params = border_enabled.then(|| BorderParams {
        color: config.border_color(),
        opacity: config.border_opacity(),
        width: config.border_width(),
        radius: focused_border_radius.unwrap_or(calculated_radius),
    });

    let dim_color = config.dim_inactive_color();
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
        info!("Cleaning up before exit");
        let ids = windows
            .iter()
            .map(|(window, _)| window.id())
            .collect::<Vec<_>>();
        window_manager.dim_windows(&ids, 0.0);
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn update_flash_messages(
    messages: Populated<(Entity, &FlashMessage, &Timeout)>,
    active_display: Single<(&Display, Entity), With<ActiveDisplayMarker>>,
    flash_mgr: Option<NonSendMut<FlashMessageManager>>,
    mut commands: Commands,
) {
    let Some(mut flash_manager) = flash_mgr else {
        return;
    };

    if messages.is_empty() {
        flash_manager.remove();
        return;
    }

    let (display, _) = *active_display;
    let bounds = display.bounds();
    let top_right = NSPoint::new(f64::from(bounds.max.x), f64::from(bounds.min.y));

    for (entity, FlashMessage(flash), timeout) in messages {
        if timeout.timer.is_finished() {
            flash_manager.remove();
            commands.entity(entity).despawn();
        } else {
            let opacity = timeout.timer.fraction_remaining();
            flash_manager.show(flash, opacity, top_right);
        }
    }
}

pub(crate) fn update_low_power_state(low_power_mode: Option<ResMut<LowPowerMode>>) {
    let Some(mut state) = low_power_mode else {
        return;
    };
    let process_info = objc2_foundation::NSProcessInfo::processInfo();
    state.0 = process_info.isLowPowerModeEnabled();
}

use bevy::app::Update;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::{ChildOf, Children};
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::query::{Has, Or, With};
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::schedule::common_conditions::resource_equals;
use bevy::ecs::system::{Commands, Local, Populated, Query, Res, ResMut};
use bevy::ecs::world::World;
use bevy::time::Time;
use log::{debug, error, info, trace, warn};
use std::time::Duration;
use stdext::function_name;

use crate::app::Application;
use crate::config::Config;
use crate::display::Display;
use crate::events::WindowManager;
use crate::events::{
    ActiveDisplayMarker, BProcess, CommandTrigger, Event, ExistingMarker, FocusedMarker,
    FreshMarker, OrphanedPane, PollForNotifications, RepositionMarker, ResizeMarker, SenderSocket,
    SpawnWindowTrigger, StrayFocusEvent, Timeout, Unmanaged, WMEventTrigger,
};
use crate::params::{ActiveDisplay, ActiveDisplayMut, ThrottledSystem};
use crate::windows::Window;

/// Registers the Bevy systems for the `WindowManager`.
///
/// # Arguments
///
/// * `app` - The Bevy application to register the systems with.
pub fn register_systems(app: &mut bevy::app::App) {
    app.add_systems(
        Update,
        (
            // NOTE: To avoid weird timing issues, the dispatcher should be the first one.
            dispatch_toplevel_triggers,
            crate::triggers::reshuffle_around_window,
            add_launched_process,
            add_launched_application,
            fresh_marker_cleanup,
            timeout_ticker,
            retry_stray_focus,
            find_orphaned_spaces,
            animate_windows,
            animate_resize_windows,
        ),
    );
    app.add_systems(
        Update,
        (display_changes_watcher, workspace_change_watcher)
            .run_if(resource_equals(PollForNotifications(true))),
    );
}

/// Processes a single incoming `Event`. It dispatches various event types to the `WindowManager` or other internal handlers.
///
/// # Arguments
///
/// * `messages` - A `MessageReader` for incoming `Event` messages.
/// * `commands` - Bevy commands to trigger events or insert resources.
#[allow(clippy::needless_pass_by_value)]
fn dispatch_toplevel_triggers(
    mut messages: MessageReader<Event>,
    mut broken_notifications: ResMut<PollForNotifications>,
    mut commands: Commands,
) {
    for event in messages.read() {
        match event {
            Event::Command { command } => commands.trigger(CommandTrigger(command.clone())),

            Event::ConfigRefresh { config } => {
                info!("{}: Configuration changed.", function_name!());
                commands.insert_resource(config.clone());
            }

            Event::WindowCreated { element } => {
                if let Ok(window) = Window::new(element).inspect_err(|err| {
                    trace!("{}: not adding window {element:?}: {err}", function_name!());
                }) {
                    commands.trigger(SpawnWindowTrigger(vec![window]));
                }
            }

            Event::SpaceChanged => {
                if broken_notifications.0 {
                    broken_notifications.0 = false;
                    info!(
                        "{}: Workspace and display notifications arriving correctly. Disabling the polling.",
                        function_name!()
                    );
                }
                commands.trigger(WMEventTrigger(event.clone()));
            }

            Event::WindowTitleChanged { window_id } => {
                trace!("{}: WindowTitleChanged: {window_id:?}", function_name!());
            }
            Event::MenuClosed { window_id } => {
                trace!("{}: MenuClosed event: {window_id:?}", function_name!());
            }
            Event::DisplayResized { display_id } => {
                debug!("{}: Display Resized: {display_id:?}", function_name!());
            }
            Event::DisplayConfigured { display_id } => {
                debug!("{}: Display Configured: {display_id:?}", function_name!());
            }
            Event::SystemWoke { msg } => {
                debug!("{}: system woke: {msg:?}", function_name!());
            }

            _ => commands.trigger(WMEventTrigger(event.clone())),
        }
    }
}

/// Runs initial setup systems in a one-shot way.
pub fn run_initial_oneshot_systems(world: &mut World) {
    let existing_apps_setup = [
        world.register_system(add_existing_process),
        world.register_system(add_existing_application),
        world.register_system(finish_setup),
    ];

    let init = existing_apps_setup
        .into_iter()
        .map(|id| world.run_system(id))
        .collect::<std::result::Result<Vec<()>, _>>();
    if let Err(err) = init {
        error!("{}: Error running initial systems: {err}", function_name!());
    }
}

/// Gathers all present displays and spawns them as entities in the Bevy world.
/// The active display is marked with `FocusedMarker`.
///
/// # Arguments
///
/// * `cid` - The main connection ID resource.
/// * `commands` - Bevy commands to spawn entities.
#[allow(clippy::needless_pass_by_value)]
pub fn gather_displays(window_manager: Res<WindowManager>, mut commands: Commands) {
    let Ok(active_display_id) = window_manager.0.active_display_id() else {
        error!("{}: Unable to get active display id!", function_name!());
        return;
    };
    for display in window_manager.0.present_displays() {
        if display.id() == active_display_id {
            commands.spawn((display, ActiveDisplayMarker));
        } else {
            commands.spawn(display);
        }
    }
}

/// Adds an existing process to the window manager. This is used during initial setup for already running applications.
/// It attempts to create and observe the application and its windows.
///
/// # Arguments
///
/// * `wm` - The `WindowManager` resource.
/// * `events` - The event sender socket resource.
/// * `process_query` - A query for existing processes marked with `ExistingMarker`.
/// * `commands` - Bevy commands to spawn entities and manage components.
#[allow(clippy::needless_pass_by_value)]
fn add_existing_process(
    wm: Res<WindowManager>,
    events: Res<SenderSocket>,
    process_query: Query<(Entity, &BProcess), With<ExistingMarker>>,
    mut commands: Commands,
) {
    for (entity, process) in process_query {
        let app = Application::new(&wm, &process.0, &events.0).unwrap();
        commands.spawn((app, ExistingMarker, ChildOf(entity)));
        commands.entity(entity).try_remove::<ExistingMarker>();
    }
}

/// Adds an existing application to the window manager. This is used during initial setup.
/// It observes the application and adds its windows.
///
/// # Arguments
///
/// * `wm` - The `WindowManager` resource.
/// * `displays` - A query for all displays.
/// * `app_query` - A query for existing applications marked with `ExistingMarker`.
/// * `commands` - Bevy commands to spawn entities and manage components.
#[allow(clippy::needless_pass_by_value)]
fn add_existing_application(
    wm: Res<WindowManager>,
    displays: Query<&Display>,
    app_query: Query<(&mut Application, Entity), With<ExistingMarker>>,
    mut commands: Commands,
) {
    let spaces = displays
        .iter()
        .flat_map(|display| display.spaces.keys().copied().collect::<Vec<_>>())
        .collect::<Vec<_>>();

    for (mut app, entity) in app_query {
        if app.observe().is_ok_and(|result| result)
            && let Ok(windows) =
                wm.0.add_existing_application_windows(&mut app, &spaces, 0)
                    .inspect_err(|err| warn!("{}: {err}", function_name!()))
        {
            commands.trigger(SpawnWindowTrigger(windows));
        }
        commands.entity(entity).try_remove::<ExistingMarker>();
    }
}

/// Finishes the initialization process once all initial windows are loaded.
///
/// # Arguments
///
/// * `apps` - A query for all applications, checking if they are still marked as fresh.
/// * `windows` - A query for all windows.
/// * `initializing` - A query for the initializing marker entity.
/// * `displays` - A query for all displays.
/// * `main_cid` - The main connection ID resource.
/// * `commands` - Bevy commands to despawn entities and send messages.
#[allow(clippy::needless_pass_by_value)]
fn finish_setup(
    mut windows: Query<(&mut Window, Entity, Has<Unmanaged>)>,
    displays: Query<(&mut Display, Has<ActiveDisplayMarker>)>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    info!(
        "{}: Finished Initialization: found {} windows.",
        function_name!(),
        windows.iter().len()
    );

    for (mut display, active) in displays {
        window_manager.0.refresh_display(&mut display, &mut windows);

        if active {
            let active_panel = window_manager
                .0
                .active_display_space(display.id())
                .and_then(|active_space| display.active_panel(active_space));

            let first_window = active_panel
                .ok()
                .and_then(|panel| panel.first().ok())
                .and_then(|panel| panel.top());
            if let Some(entity) = first_window {
                debug!("{}: focusing {entity}", function_name!());
                commands.entity(entity).try_insert(FocusedMarker);
            }
        }
    }
}

/// Handles the event when a new application is launched. It creates a `Process` and `Application` object,
/// observes the application for events, and adds its windows to the manager.
///
/// # Arguments
///
/// * `wm` - The `WindowManager` resource.
/// * `events` - The event sender socket resource.
/// * `process_query` - A query for newly launched processes marked with `FreshMarker`.
/// * `commands` - Bevy commands to spawn entities and manage components.
#[allow(clippy::needless_pass_by_value)]
fn add_launched_process(
    wm: Res<WindowManager>,
    events: Res<SenderSocket>,
    process_query: Populated<(Entity, &mut BProcess, Has<Children>), With<FreshMarker>>,
    mut commands: Commands,
) {
    const APP_OBSERVABLE_TIMEOUT_SEC: u64 = 5;
    for (entity, mut process, children) in process_query {
        let process = &mut *process.0;
        if !process.ready() {
            continue;
        }

        if children {
            // Process already has an attached Application, so finish.
            commands.entity(entity).try_remove::<FreshMarker>();
            continue;
        }

        let mut app = Application::new(&wm, process, &events.0).unwrap();

        if app.observe().is_ok_and(|good| good) {
            let timeout = Timeout::new(
                Duration::from_secs(APP_OBSERVABLE_TIMEOUT_SEC),
                Some(format!(
                    "{}: {app} did not become observable in {APP_OBSERVABLE_TIMEOUT_SEC}s.",
                    function_name!(),
                )),
            );
            commands.spawn((app, FreshMarker, timeout, ChildOf(entity)));
        } else {
            debug!(
                "{}: failed to register some observers {}",
                function_name!(),
                process.name
            );
        }
    }
}

/// Adds windows for a newly launched application.
///
/// # Arguments
///
/// * `app_query` - A query for newly launched applications marked with `FreshMarker`.
/// * `windows` - A query for all windows.
/// * `commands` - Bevy commands to spawn entities and manage components.
fn add_launched_application(
    app_query: Populated<(&mut Application, Entity), With<FreshMarker>>,
    windows: Query<&Window>,
    mut commands: Commands,
) {
    // TODO: maybe refactor this with add_existing_application_windows()
    let find_window = |window_id| windows.iter().find(|window| window.id() == window_id);

    for (app, entity) in app_query {
        let Ok(app_windows) = app.window_list() else {
            continue;
        };
        let create_windows = app_windows
            .into_iter()
            .filter_map(|window| {
                window
                    .inspect_err(|err| warn!("{}: error adding window: {err}", function_name!()))
                    .ok()
                    .and_then(|window| {
                        // Window does not exist, create it.
                        find_window(window.id()).is_none().then_some(window)
                    })
            })
            .collect();
        commands.entity(entity).try_remove::<FreshMarker>();
        commands.trigger(SpawnWindowTrigger(create_windows));
    }
}

/// Cleans up entities which have been initializing for too long.
///
/// This can be processes which are not yet observable or applications which keep failing to
/// register some of the observers.
#[allow(clippy::type_complexity)]
fn fresh_marker_cleanup(
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

/// A Bevy system that ticks timers and despawns entities when their timers finish.
#[allow(clippy::needless_pass_by_value)]
fn timeout_ticker(
    timers: Populated<(Entity, &mut Timeout)>,
    clock: Res<Time>,
    mut commands: Commands,
) {
    for (entity, mut timeout) in timers {
        if timeout.timer.is_finished() {
            trace!(
                "{}: Despawning entity {entity} due to timeout.",
                function_name!(),
            );
            if let Some(message) = &timeout.message {
                debug!("{message}");
            }
            trace!("{}: Removing timer {entity}", function_name!());
            commands.entity(entity).despawn();
        } else {
            trace!(
                "{}: Timer {}",
                function_name!(),
                timeout.timer.elapsed().as_secs_f32()
            );
            timeout.timer.tick(clock.delta());
        }
    }
}

/// A Bevy system that retries focusing a window if the focus event arrived before the window was created.
fn retry_stray_focus(
    focus_events: Populated<(Entity, &StrayFocusEvent)>,
    windows: Query<&Window>,
    mut messages: MessageWriter<Event>,
    mut commands: Commands,
) {
    for (timeout_entity, stray_focus) in focus_events {
        let window_id = stray_focus.0;
        if windows.iter().any(|window| window.id() == window_id) {
            debug!(
                "{}: Re-queueing lost focus event for window id {window_id}.",
                function_name!()
            );
            messages.write(Event::WindowFocused { window_id });
            commands.entity(timeout_entity).despawn();
        }
    }
}

/// A Bevy system that finds and re-assigns orphaned spaces to the active display.
#[allow(clippy::needless_pass_by_value)]
fn find_orphaned_spaces(
    orphaned_spaces: Populated<(Entity, &mut OrphanedPane)>,
    mut active_display: ActiveDisplayMut,
    mut commands: Commands,
) {
    let active_display_id = active_display.id();

    for (entity, orphan_pane) in orphaned_spaces {
        debug!(
            "{}: Checking orphaned pane {}",
            function_name!(),
            orphan_pane.id
        );
        for (space_id, pane) in &mut active_display.display().spaces {
            if *space_id == orphan_pane.id {
                debug!(
                    "{}: Re-inserting orphaned pane {} into display {}",
                    function_name!(),
                    orphan_pane.id,
                    active_display_id
                );

                for window_entity in orphan_pane.pane.all_windows() {
                    // TODO: check for clashing windows.
                    pane.append(window_entity);
                }

                commands.entity(entity).despawn();
            }
        }
    }
}

/// Periodically checks for displays added and removed.
/// TODO: Workaround for Tahoe 26.x, where display change notifications are not arriving.
#[allow(clippy::needless_pass_by_value)]
fn display_changes_watcher(
    displays: Query<(&Display, Has<ActiveDisplayMarker>)>,
    window_manager: Res<WindowManager>,
    mut throttle: ThrottledSystem,
    mut commands: Commands,
) {
    const DISPLAY_CHANGE_CHECK_FREQ_MS: u64 = 1000;
    if throttle.throttled(Duration::from_millis(DISPLAY_CHANGE_CHECK_FREQ_MS)) {
        return;
    }

    let Ok(current_display_id) = window_manager.0.active_display_id() else {
        return;
    };
    let found = displays
        .iter()
        .find(|(display, _)| display.id() == current_display_id);
    if let Some((_, active)) = found {
        if active {
            return;
        }
        debug!(
            "{}: detected dislay change from {}.",
            function_name!(),
            current_display_id,
        );
        commands.trigger(WMEventTrigger(Event::DisplayChanged));
    } else {
        debug!(
            "{}: new display {} detected.",
            function_name!(),
            current_display_id
        );
        commands.trigger(WMEventTrigger(Event::DisplayAdded {
            display_id: current_display_id,
        }));
    }

    let present_displays = window_manager.0.present_displays();
    displays.iter().for_each(|(display, _)| {
        if !present_displays
            .iter()
            .any(|present_display| present_display.id() == display.id())
        {
            debug!(
                "{}: detected removal of display {}",
                function_name!(),
                display.id()
            );
            commands.trigger(WMEventTrigger(Event::DisplayRemoved {
                display_id: display.id(),
            }));
        }
    });
}

/// Periodically checks for windows moved between spaces and displays.
/// TODO: Workaround for Tahoe 26.x, where workspace notifications are not arriving. So if a
/// window is missing in the current space, try to trigger a workspace change event.
#[allow(clippy::needless_pass_by_value)]
fn workspace_change_watcher(
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    mut throttle: ThrottledSystem,
    mut current_space: Local<u64>,
    mut commands: Commands,
) {
    const WORKSPACE_CHANGE_FREQ_MS: u64 = 1000;
    if throttle.throttled(Duration::from_millis(WORKSPACE_CHANGE_FREQ_MS)) {
        return;
    }

    let Ok(space_id) = window_manager
        .0
        .active_display_space(active_display.id())
        .inspect_err(|err| warn!("{}: {err}", function_name!()))
    else {
        return;
    };

    if *current_space != space_id {
        *current_space = space_id;
        debug!("{}: workspace changed to {space_id}", function_name!());
        commands.trigger(WMEventTrigger(Event::SpaceChanged));
    }
}

/// Animates window movement.
/// This is a Bevy system that runs on `Update`. It smoothly moves windows to their target
/// positions, as indicated by the `RepositionMarker` component.
///
/// # Arguments
///
/// * `windows` - A query for windows with a `RepositionMarker`.
/// * `displays` - A query for the active display.
/// * `time` - The Bevy `Time` resource.
/// * `config` - The optional configuration resource, used for animation speed.
/// * `commands` - Bevy commands to remove the `RepositionMarker` when animation is complete.
#[allow(clippy::needless_pass_by_value)]
fn animate_windows(
    windows: Populated<(&mut Window, Entity, &RepositionMarker)>,
    displays: Query<&Display>,
    time: Res<Time>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let move_speed = config
        .options()
        .animation_speed
        // If unset, set it to something high, so the move happens immediately,
        // effectively disabling animation.
        .unwrap_or(1_000_000.0)
        .max(500.0);
    let move_delta = move_speed * time.delta_secs_f64();

    for (mut window, entity, RepositionMarker { origin, display_id }) in windows {
        let Some(display) = displays.iter().find(|display| display.id() == *display_id) else {
            continue;
        };
        let current = window.frame().origin;
        let mut delta_x = (origin.x - current.x).abs().min(move_delta);
        let mut delta_y = (origin.y - current.y).abs().min(move_delta);
        if delta_x < move_delta && delta_y < move_delta {
            commands.entity(entity).try_remove::<RepositionMarker>();
            window.reposition(
                origin.x,
                origin.y.max(display.menubar_height),
                &display.bounds,
            );
            continue;
        }

        if origin.x < current.x {
            delta_x = -delta_x;
        }
        if origin.y < current.y {
            delta_y = -delta_y;
        }
        trace!(
            "{}: window {} dest {:?} delta {move_delta:.0} moving to {:.0}:{:.0}",
            function_name!(),
            window.id(),
            origin,
            current.x + delta_x,
            current.y + delta_y,
        );
        window.reposition(
            current.x + delta_x,
            (current.y + delta_y).max(display.menubar_height),
            &display.bounds,
        );
    }
}

/// Animates window resizing.
/// This is a Bevy system that runs on `Update`. It resizes windows to their target
/// dimensions, as indicated by the `ResizeMarker` component.
///
/// # Arguments
///
/// * `windows` - A query for windows with a `ResizeMarker`.
/// * `displays` - A query for the active display.
/// * `commands` - Bevy commands to remove the `ResizeMarker` when resizing is complete.
#[allow(clippy::needless_pass_by_value)]
fn animate_resize_windows(
    windows: Populated<(&mut Window, Entity, &ResizeMarker)>,
    active_display: ActiveDisplay,
    mut commands: Commands,
) {
    for (mut window, entity, ResizeMarker { size }) in windows {
        let origin = window.frame().origin;
        let width = if origin.x + size.width < active_display.bounds().size.width + 0.4 {
            commands.entity(entity).try_remove::<ResizeMarker>();
            size.width
        } else {
            active_display.bounds().size.width - origin.x
        };
        debug!(
            "{}: window {} resize {}:{}",
            function_name!(),
            window.id(),
            width,
            size.height,
        );
        window.resize(width, size.height, &active_display.bounds());
    }
}

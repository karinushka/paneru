use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::{ChildOf, Children};
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::query::{Has, Or, With};
use bevy::ecs::system::{Commands, Local, Populated, Query, Res, ResMut};
use bevy::ecs::world::World;
use bevy::time::Time;
use log::{debug, error, info, trace, warn};
use objc2_core_foundation::CGPoint;
use std::time::Duration;
use stdext::function_name;

use super::{
    ActiveDisplayMarker, BProcess, CommandTrigger, ExistingMarker, FocusedMarker, FreshMarker,
    OrphanedPane, PollForNotifications, RepositionMarker, ResizeMarker, SpawnWindowTrigger,
    StrayFocusEvent, Timeout, Unmanaged, WMEventTrigger,
};
use crate::config::Config;
use crate::ecs::params::{
    ActiveDisplay, ActiveDisplayMut, Configuration, DebouncedSystem, ThrottledSystem,
};
use crate::ecs::{ReshuffleAroundMarker, WindowSwipeMarker};
use crate::events::Event;
use crate::manager::{Application, Display, Panel, Window, WindowManager, WindowOS, WindowPane};

const WINDOW_HIDDEN_THRESHOLD: f64 = 10.0;

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
    mut broken_notifications: ResMut<PollForNotifications>,
    mut commands: Commands,
) {
    for event in messages.read() {
        match event {
            Event::Command { command } => commands.trigger(CommandTrigger(command.clone())),

            Event::WindowCreated { element } => {
                if let Ok(window) = WindowOS::new(element)
                    .inspect_err(|err| {
                        trace!("{}: not adding window {element:?}: {err}", function_name!());
                    })
                    .map(|window| Window::new(Box::new(window)))
                {
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
/// This function registers and runs systems that are crucial for the initial state setup of the Bevy world,
/// such as adding existing processes and applications.
///
/// # Arguments
///
/// * `world` - The Bevy `World` instance to run the systems on.
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
/// The currently active display (identified by `window_manager.active_display_id()`) is marked with `ActiveDisplayMarker`.
///
/// # Arguments
///
/// * `window_manager` - The `WindowManager` resource for querying display information.
/// * `commands` - Bevy commands to spawn entities.
#[allow(clippy::needless_pass_by_value)]
pub fn gather_displays(window_manager: Res<WindowManager>, mut commands: Commands) {
    let Ok(active_display_id) = window_manager.active_display_id() else {
        error!("{}: Unable to get active display id!", function_name!());
        return;
    };
    for display in window_manager.present_displays() {
        if display.id() == active_display_id {
            commands.spawn((display, ActiveDisplayMarker));
        } else {
            commands.spawn(display);
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
fn add_existing_process(
    window_manager: Res<WindowManager>,
    process_query: Query<(Entity, &BProcess), With<ExistingMarker>>,
    mut commands: Commands,
) {
    for (entity, process) in process_query {
        let app = window_manager.new_application(&*process.0).unwrap();
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
fn add_existing_application(
    window_manager: Res<WindowManager>,
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
            && let Ok(windows) = window_manager
                .add_existing_application_windows(&mut app, &spaces, 0)
                .inspect_err(|err| warn!("{}: {err}", function_name!()))
        {
            commands.trigger(SpawnWindowTrigger(windows));
        }
        commands.entity(entity).try_remove::<ExistingMarker>();
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
        window_manager.refresh_display(&mut display, &mut windows);

        if active {
            let active_panel = window_manager
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

        let mut app = window_manager.new_application(process).unwrap();

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
                process.name()
            );
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
pub(super) fn add_launched_application(
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

/// A Bevy system that retries focusing a window if a `StrayFocusEvent` arrived before the window was created.
/// If the window is now present in the `World`, the `WindowFocused` event is re-queued, and the `StrayFocusEvent` entity is despawned.
///
/// # Arguments
///
/// * `focus_events` - A `Populated` query for `(Entity, &StrayFocusEvent)` components.
/// * `windows` - A query for all `Window` components, used to check for the existence of the target window.
/// * `messages` - A `MessageWriter` for sending new `Event` messages.
/// * `commands` - Bevy commands to despawn entities.
pub(super) fn retry_stray_focus(
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
/// This system iterates through `OrphanedPane` entities, attempts to merge their windows into an existing space on the active display,
/// and then despawns the `OrphanedPane` entity.
///
/// # Arguments
///
/// * `orphaned_spaces` - A `Populated` query for `(Entity, &mut OrphanedPane)` components.
/// * `active_display` - A mutable `ActiveDisplayMut` system parameter for the currently active display.
/// * `commands` - Bevy commands to despawn entities.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn find_orphaned_spaces(
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
    mut throttle: ThrottledSystem,
    mut commands: Commands,
) {
    const DISPLAY_CHANGE_CHECK_FREQ_MS: u64 = 1000;
    if throttle.throttled(Duration::from_millis(DISPLAY_CHANGE_CHECK_FREQ_MS)) {
        return;
    }

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

    let present_displays = window_manager.present_displays();
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
    // _: Single<&Window, With<FocusedMarker>>, // Will only run if there's a focused window.
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
pub(super) fn animate_windows(
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
/// When a window reaches its target size, the `ResizeMarker` is removed.
///
/// # Arguments
///
/// * `windows` - A `Populated` query for `(&mut Window, Entity, &ResizeMarker)` components.
/// * `active_display` - An `ActiveDisplay` system parameter providing immutable access to the active display.
/// * `commands` - Bevy commands to remove the `ResizeMarker` when resizing is complete.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn animate_resize_windows(
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

#[allow(clippy::needless_pass_by_value)]
pub(super) fn window_swiper(
    sliding: Populated<(&Window, Entity, &WindowSwipeMarker)>,
    windows: Query<(&Window, Entity)>,
    active_display: ActiveDisplay,
    config: Configuration,
    mut debouncer: DebouncedSystem,
    mut commands: Commands,
) {
    const DEBOUNCE_SWIPE_EVENTS_MS: u64 = 500;
    for (window, entity, WindowSwipeMarker(delta)) in sliding {
        commands.entity(entity).try_remove::<WindowSwipeMarker>();

        let pos_x = window.frame().origin.x - (active_display.bounds().size.width * delta);
        let frame = window.frame();

        commands.entity(entity).try_insert(RepositionMarker {
            origin: CGPoint {
                x: pos_x
                    .min(active_display.bounds().size.width - frame.size.width)
                    .max(0.0),
                y: frame.origin.y,
            },
            display_id: active_display.id(),
        });

        if pos_x > 0.0 && pos_x < (active_display.bounds().size.width - frame.size.width) {
            commands.entity(entity).try_insert(ReshuffleAroundMarker);
            return;
        }

        if !config.continuous_swipe()
            || debouncer.bounce(Duration::from_millis(DEBOUNCE_SWIPE_EVENTS_MS))
        {
            return;
        }

        if let Some((window, _)) =
            slide_to_next_window(&active_display, entity, *delta, pos_x, &mut commands)
                .and_then(|entity| windows.get(entity).ok())
        {
            commands.trigger(WMEventTrigger(Event::WindowFocused {
                window_id: window.id(),
            }));
        }
    }
}

fn slide_to_next_window(
    active_display: &ActiveDisplay,
    entity: Entity,
    delta: f64,
    delta_x: f64,
    commands: &mut Commands,
) -> Option<Entity> {
    let Ok(pane) = active_display.active_panel() else {
        return None;
    };
    let neighbour = if delta_x < 0.0 {
        pane.right_neighbour(entity)
    } else {
        pane.left_neighbour(entity)
    };

    neighbour.inspect(|neighbour| {
        debug!(
            "{}: switching to {neighbour} with delta {delta}",
            function_name!()
        );
        commands
            .entity(*neighbour)
            .try_insert(ReshuffleAroundMarker);
    })
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
pub(super) fn reshuffle_around_window(
    active_display: ActiveDisplay,
    marker: Populated<Entity, With<ReshuffleAroundMarker>>,
    mut windows: Query<(&mut Window, Entity, Option<&RepositionMarker>)>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    if window_manager
        .0
        .active_display_id()
        .is_ok_and(|id| active_display.id() != id)
    {
        debug!("{}: detected display change.", function_name!());
        commands.trigger(WMEventTrigger(Event::DisplayChanged));
        return;
    }

    let shuffled = marker.into_iter().collect::<Vec<_>>();
    for entity in shuffled {
        let Ok((window, _, moving)) = windows.get(entity) else {
            continue;
        };

        if let Ok(mut cmd) = commands.get_entity(entity) {
            cmd.try_remove::<ReshuffleAroundMarker>();
        }
        let display_bounds = active_display.bounds();
        let Ok(active_panel) = active_display.active_panel() else {
            return;
        };

        let frame = window.expose_window(&active_display, moving, entity, &mut commands);
        let window_width = |entity| {
            windows
                .get(entity)
                .ok()
                .map(|window| window.0.frame().size.width)
        };

        let Some(positions) = calculate_positions(
            entity,
            frame.origin.x,
            display_bounds.size.width,
            active_panel,
            &window_width,
        ) else {
            return;
        };
        let positions = positions
            .zip(active_panel.all_columns())
            .map(|(position, entity)| (position, entity, window_width(entity).unwrap()))
            .collect::<Vec<_>>();

        for (upper_left, entity, width) in positions {
            let Ok(panel) = active_panel
                .index_of(entity)
                .and_then(|index| active_panel.get(index))
            else {
                continue;
            };

            reposition_stack(
                upper_left,
                &panel,
                width,
                &active_display,
                &mut windows,
                &mut commands,
            );
        }
    }
}

pub fn absolute_positions<W>(
    active_panel: &WindowPane,
    window_width: W,
) -> impl Iterator<Item = f64>
where
    W: Fn(Entity) -> Option<f64>,
{
    let mut left_edge = 0.0;

    active_panel
        .all_columns()
        .into_iter()
        .filter_map(window_width)
        .map(move |width| {
            let temp = left_edge;
            left_edge += width;
            temp
        })
}

fn calculate_positions<W>(
    entity: Entity,
    current_x: f64,
    display_width: f64,
    active_panel: &WindowPane,
    window_width: W,
) -> Option<impl Iterator<Item = f64>>
where
    W: Fn(Entity) -> Option<f64>,
{
    let widths = active_panel
        .all_columns()
        .into_iter()
        .filter_map(&window_width)
        .collect::<Vec<_>>();
    let positions = absolute_positions(active_panel, &window_width).collect::<Vec<_>>();
    let offset = active_panel
        .index_of(entity)
        .ok()
        .and_then(|index| positions.get(index))
        .map(|offset| current_x - offset)?;

    Some(
        positions
            .into_iter()
            .zip(widths)
            .map(move |(position, width)| {
                let left_edge = position + offset;
                if left_edge + width < 0.0 {
                    0.0 - width + WINDOW_HIDDEN_THRESHOLD
                } else if left_edge > display_width - WINDOW_HIDDEN_THRESHOLD {
                    display_width - WINDOW_HIDDEN_THRESHOLD
                } else {
                    left_edge
                }
            }),
    )
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
    active_display: &ActiveDisplay,
    windows: &mut Query<(&mut Window, Entity, Option<&RepositionMarker>)>,
    commands: &mut Commands,
) {
    const REMAINING_THERSHOLD: f64 = 200.0;
    let display_height =
        active_display.bounds().size.height - active_display.display().menubar_height;
    let entities = match panel {
        Panel::Single(entity) => vec![*entity],
        Panel::Stack(stack) => stack.clone(),
    };
    let count: f64 = u32::try_from(entities.len()).unwrap().into();
    let mut fits = 0f64;
    let mut height = active_display.display().menubar_height;
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
            // window.reposition(upper_left, y_pos, display_bounds);
            commands.entity(entity).try_insert(RepositionMarker {
                origin: CGPoint {
                    x: upper_left,
                    y: y_pos,
                },
                display_id: active_display.id(),
            });
            if fits > 0.0 {
                y_pos += window_height;
                fits -= 1.0;
            } else {
                window.resize(width, avg_height, &active_display.bounds());
                y_pos += avg_height;
            }
        }
    }
}

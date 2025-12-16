use bevy::app::Update;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::{ChildOf, Children};
use bevy::ecs::message::MessageWriter;
use bevy::ecs::query::{Has, Or, With};
use bevy::ecs::system::{Commands, Populated, Query, Res, Single};
use bevy::time::{Time, Virtual};
use core::ptr::NonNull;
use log::{debug, error, trace, warn};
use objc2_core_foundation::{CFArray, CFMutableData, CFNumberType, CFRetained};
use std::io::ErrorKind;
use std::ops::Deref;
use std::slice::from_raw_parts_mut;
use std::time::Duration;
use stdext::function_name;

use crate::app::Application;
use crate::errors::{Error, Result};
use crate::events::{
    BProcess, Event, ExistingMarker, FocusedMarker, FreshMarker, MainConnection, OrphanedPane,
    SenderSocket, SpawnWindowTrigger, StrayFocusEvent, Timeout,
};
use crate::skylight::{
    _AXUIElementCreateWithRemoteToken, ConnID, SLSCopyWindowsWithOptionsAndTags,
    SLSWindowIteratorAdvance, SLSWindowIteratorGetAttributes, SLSWindowIteratorGetParentID,
    SLSWindowIteratorGetTags, SLSWindowIteratorGetWindowID, SLSWindowQueryResultCopyWindows,
    SLSWindowQueryWindows, WinID,
};
use crate::util::{AXUIWrapper, create_array, get_array_values};
use crate::windows::{Display, Window, ax_window_id};

/// The main window manager logic.
///
/// This struct contains the Bevy systems that respond to events and manage windows.
#[derive(Default)]
pub struct WindowManager;

impl WindowManager {
    pub fn register_systems(app: &mut bevy::app::App) {
        app.add_systems(
            Update,
            (
                WindowManager::add_launched_process,
                WindowManager::add_launched_application,
                WindowManager::fresh_marker_cleanup,
                WindowManager::timeout_ticker,
                WindowManager::retry_stray_focus,
                WindowManager::find_orphaned_spaces,
            ),
        );
    }

    /// Refreshes the list of active displays and reorganizes windows across them.
    /// It preserves spaces from old displays if they still exist.
    ///
    /// # Arguments
    ///
    /// * `main_cid` - The main connection ID.
    /// * `display` - The display to refresh.
    /// * `windows` - A mutable query for all `Window` components.
    pub fn refresh_display(
        main_cid: ConnID,
        display: &mut Display,
        windows: &mut Query<(&mut Window, Entity)>,
    ) {
        debug!(
            "{}: Refreshing windows on display {}",
            function_name!(),
            display.id
        );

        let display_bounds = display.bounds;
        for (space_id, pane) in &mut display.spaces {
            let mut lens = windows.transmute_lens::<(&Window, Entity)>();
            let new_windows = refresh_windows_space(main_cid, *space_id, &lens.query());

            // Preserve the order - do not flush existing windows.
            for window_entity in pane.all_windows() {
                if !new_windows.contains(&window_entity) {
                    pane.remove(window_entity);
                }
            }
            for window_entity in new_windows {
                if windows
                    .get(window_entity)
                    .is_ok_and(|(window, _)| !window.managed())
                {
                    // Window is not managed, do not insert it into the panel.
                    continue;
                }
                if pane.index_of(window_entity).is_err() {
                    pane.append(window_entity);
                    if let Ok((mut window, _)) = windows.get_mut(window_entity) {
                        _ = window.update_frame(Some(&display_bounds));
                    }
                }
            }
            debug!(
                "{}: space {space_id}: after refresh {pane}",
                function_name!()
            );
        }
    }

    /// Adds an existing process to the window manager. This is used during initial setup for already running applications.
    /// It attempts to create and observe the application and its windows.
    ///
    /// # Arguments
    ///
    /// * `cid` - The main connection ID resource.
    /// * `events` - The event sender socket resource.
    /// * `process_query` - A query for existing processes marked with `ExistingMarker`.
    /// * `commands` - Bevy commands to spawn entities and manage components.
    #[allow(clippy::needless_pass_by_value)]
    pub fn add_existing_process(
        cid: Res<MainConnection>,
        events: Res<SenderSocket>,
        process_query: Query<(Entity, &BProcess), With<ExistingMarker>>,
        mut commands: Commands,
    ) {
        for (entity, process) in process_query {
            let app = Application::new(cid.0, &process.0, &events.0).unwrap();
            commands.spawn((app, ExistingMarker, ChildOf(entity)));
            commands.entity(entity).remove::<ExistingMarker>();
        }
    }

    /// Adds an existing application to the window manager. This is used during initial setup.
    /// It observes the application and adds its windows.
    ///
    /// # Arguments
    ///
    /// * `cid` - The main connection ID resource.
    /// * `displays` - A query for all displays.
    /// * `app_query` - A query for existing applications marked with `ExistingMarker`.
    /// * `windows` - A query for all `Window` components.
    /// * `commands` - Bevy commands to spawn entities and manage components.
    #[allow(clippy::needless_pass_by_value)]
    pub fn add_existing_application(
        cid: Res<MainConnection>,
        displays: Query<&Display>,
        app_query: Query<(&mut Application, Entity), With<ExistingMarker>>,
        windows: Query<(&Window, Entity)>,
        mut commands: Commands,
    ) {
        let spaces = displays
            .iter()
            .flat_map(|display| display.spaces.keys().copied().collect::<Vec<_>>())
            .collect::<Vec<_>>();

        for (mut app, entity) in app_query {
            if app.observe().is_ok_and(|result| result)
                && let Ok(windows) =
                    add_existing_application_windows(cid.0, &mut app, &spaces, 0, &windows)
                        .inspect_err(|err| warn!("{}: {err}", function_name!()))
            {
                commands.trigger(SpawnWindowTrigger(windows));
            }
            commands.entity(entity).remove::<ExistingMarker>();
        }
    }

    /// Handles the event when a new application is launched. It creates a `Process` and `Application` object,
    /// observes the application for events, and adds its windows to the manager.
    ///
    /// # Arguments
    ///
    /// * `cid` - The main connection ID resource.
    /// * `events` - The event sender socket resource.
    /// * `process_query` - A query for newly launched processes marked with `FreshMarker`.
    /// * `time` - The Bevy time resource.
    /// * `commands` - Bevy commands to spawn entities and manage components.
    #[allow(clippy::needless_pass_by_value)]
    pub fn add_launched_process(
        cid: Res<MainConnection>,
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
                commands.entity(entity).remove::<FreshMarker>();
                continue;
            }

            let mut app = Application::new(cid.0, process, &events.0).unwrap();

            if app.observe().is_ok_and(|good| good) {
                let timeout = Timeout::new(
                    Duration::from_secs(APP_OBSERVABLE_TIMEOUT_SEC),
                    Some(format!(
                        "{}: Application pid {} did not become observable in {APP_OBSERVABLE_TIMEOUT_SEC}s.",
                        function_name!(),
                        app.pid()
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
    pub fn add_launched_application(
        app_query: Populated<(&mut Application, Entity), With<FreshMarker>>,
        windows: Query<&Window>,
        mut commands: Commands,
    ) {
        // TODO: maybe refactor this with add_existing_application_windows()
        let find_window = |window_id| windows.iter().find(|window| window.id() == window_id);

        for (app, entity) in app_query {
            let Ok(array) = app.window_list() else {
                continue;
            };
            let create_window = |element_ref: NonNull<_>| {
                let element = AXUIWrapper::retain(element_ref.as_ptr()).ok();
                element.and_then(|element| {
                    let window_id = ax_window_id(element.as_ptr())
                        .inspect_err(|err| {
                            warn!("{}: error adding window: {err}", function_name!());
                        })
                        .ok()?;
                    if find_window(window_id).is_none() {
                        // Window does not exist, create it.
                        Some(Window::new(&element).inspect_err(|err| {
                            warn!("{}: error adding window: {err}.", function_name!());
                        }))
                    } else {
                        // Window already exists, skip it.
                        None
                    }
                })
            };
            let windows = get_array_values::<accessibility_sys::__AXUIElement>(&array)
                .filter_map(create_window)
                .flatten()
                .collect::<Vec<_>>();
            commands.entity(entity).remove::<FreshMarker>();
            commands.trigger(SpawnWindowTrigger(windows));
        }
    }

    /// Cleans up entities which have been initializing for too long.
    ///
    /// This can be processes which are not yet observable or applications which keep failing to
    /// register some of the observers.
    #[allow(clippy::type_complexity)]
    pub fn fresh_marker_cleanup(
        cleanup: Populated<
            (Entity, Has<FreshMarker>, &Timeout),
            Or<(With<BProcess>, With<Application>)>,
        >,
        mut commands: Commands,
    ) {
        for (entity, fresh, _) in cleanup {
            if !fresh {
                // Process was ready before the timer finished.
                commands.entity(entity).remove::<Timeout>();
            }
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn timeout_ticker(
        timers: Populated<(Entity, &mut Timeout)>,
        clock: Res<Time<Virtual>>,
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

    pub fn retry_stray_focus(
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

    #[allow(clippy::needless_pass_by_value)]
    pub fn find_orphaned_spaces(
        orphaned_spaces: Populated<(Entity, &mut OrphanedPane)>,
        mut active_display: Single<&mut Display, With<FocusedMarker>>,
        mut commands: Commands,
    ) {
        let display_id = active_display.id;

        for (entity, orphan_pane) in orphaned_spaces {
            debug!(
                "{}: Checking orphaned pane {}",
                function_name!(),
                orphan_pane.id
            );
            for (space_id, pane) in &mut active_display.spaces {
                if *space_id == orphan_pane.id {
                    debug!(
                        "{}: Re-inserting orphaned pane {} into display {}",
                        function_name!(),
                        orphan_pane.id,
                        display_id
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
}

/// Repopulates the current window panel with eligible windows from a specified space.
///
/// # Arguments
///
/// * `main_cid` - The main connection ID.
/// * `space_id` - The ID of the space to refresh windows from.
/// * `windows` - A query for all windows.
fn refresh_windows_space(
    main_cid: ConnID,
    space_id: u64,
    windows: &Query<(&Window, Entity)>,
) -> Vec<Entity> {
    space_window_list_for_connection(main_cid, &[space_id], None, false, windows)
        .inspect_err(|err| {
            warn!(
                "{}: getting windows for space {space_id}: {err}",
                function_name!()
            );
        })
        .unwrap_or_default()
        .into_iter()
        .filter_map(|window_id| windows.iter().find(|(window, _)| window.id() == window_id))
        .filter_map(|(window, entity)| window.is_eligible().then_some(entity))
        .collect()
}

/// Retrieves a list of window IDs for specified spaces and connection, with an option to include minimized windows.
/// This function uses `SkyLight` API calls.
///
/// # Arguments
///
/// * `main_cid` - The main connection ID.
/// * `spaces` - A slice of space IDs to query windows from.
/// * `cid` - An optional connection ID. If `None`, the main connection ID is used.
/// * `also_minimized` - A boolean indicating whether to include minimized windows in the result.
/// * `windows` - A query for all windows.
///
/// # Returns
///
/// `Ok(Vec<WinID>)` containing the list of window IDs if successful, otherwise `Err(Error)`.
fn space_window_list_for_connection(
    main_cid: ConnID,
    spaces: &[u64],
    cid: Option<ConnID>,
    also_minimized: bool,
    windows: &Query<(&Window, Entity)>,
) -> Result<Vec<WinID>> {
    unsafe {
        let space_list_ref = create_array(spaces, CFNumberType::SInt64Type)?;

        let mut set_tags = 0i64;
        let mut clear_tags = 0i64;
        let options = if also_minimized { 0x7 } else { 0x2 };
        let ptr = NonNull::new(SLSCopyWindowsWithOptionsAndTags(
            main_cid,
            cid.unwrap_or(0),
            &raw const *space_list_ref,
            options,
            &mut set_tags,
            &mut clear_tags,
        ))
        .ok_or(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "{}: nullptr returned from SLSCopyWindowsWithOptionsAndTags.",
                function_name!()
            ),
        ))?;
        let window_list_ref = CFRetained::from_raw(ptr);

        let count = window_list_ref.count();
        if count == 0 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("{}: zero windows returned", function_name!()),
            ));
        }

        let query = CFRetained::from_raw(SLSWindowQueryWindows(
            main_cid,
            &raw const *window_list_ref,
            count,
        ));
        let iterator = CFRetained::from_raw(SLSWindowQueryResultCopyWindows(query.deref().into()));

        let mut window_list = Vec::with_capacity(count.try_into().unwrap());
        while SLSWindowIteratorAdvance(&raw const *iterator) {
            let tags = SLSWindowIteratorGetTags(&raw const *iterator);
            let attributes = SLSWindowIteratorGetAttributes(&raw const *iterator);
            let parent_wid: WinID = SLSWindowIteratorGetParentID(&raw const *iterator);
            let window_id: WinID = SLSWindowIteratorGetWindowID(&raw const *iterator);

            trace!(
                "{}: id: {window_id} parent: {parent_wid} tags: 0x{tags:x} attributes: 0x{attributes:x}",
                function_name!()
            );
            match windows.iter().find(|(window, _)| window.id() == window_id) {
                Some((window, _)) => {
                    if also_minimized || !window.minimized {
                        window_list.push(window.id());
                    }
                }
                None => {
                    if found_valid_window(parent_wid, attributes, tags) {
                        window_list.push(window_id);
                    }
                }
            }
        }
        Ok(window_list)
    }
}

/// Determines if a window is valid based on its parent ID, attributes, and tags.
/// This function implements complex logic to filter out irrelevant or invalid windows.
///
/// # Arguments
///
/// * `parent_wid` - The parent window ID.
/// * `attributes` - The attributes of the window.
/// * `tags` - The tags associated with the window.
///
/// # Returns
///
/// `true` if the window is considered valid, `false` otherwise.
fn found_valid_window(parent_wid: WinID, attributes: i64, tags: i64) -> bool {
    parent_wid == 0
        && ((0 != (attributes & 0x2) || 0 != (tags & 0x0400_0000_0000_0000))
            && (0 != (tags & 0x1) || (0 != (tags & 0x2) && 0 != (tags & 0x8000_0000))))
        || ((attributes == 0x0 || attributes == 0x1)
            && (0 != (tags & 0x1000_0000_0000_0000) || 0 != (tags & 0x0300_0000_0000_0000))
            && (0 != (tags & 0x1) || (0 != (tags & 0x2) && 0 != (tags & 0x8000_0000))))
}

/// Retrieves a list of existing application window IDs for a given application.
/// It queries windows across all active displays and spaces associated with the application's connection.
///
/// # Arguments
///
/// * `cid` - The connection ID.
/// * `app` - A reference to the `Application` for which to retrieve window IDs.
/// * `spaces` - A slice of space IDs to query.
/// * `windows` - A query for all windows.
///
/// # Returns
///
/// `Ok(Vec<WinID>)` containing the list of window IDs if successful, otherwise `Err(Error)`.
fn existing_application_window_list(
    cid: ConnID,
    app: &Application,
    spaces: &[u64],
    windows: &Query<(&Window, Entity)>,
) -> Result<Vec<WinID>> {
    if spaces.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("{}: no spaces returned", function_name!()),
        ));
    }
    space_window_list_for_connection(cid, spaces, app.connection(), true, windows)
}

/// Attempts to find and add unresolved windows for a given application by brute-forcing `element_id` values.
/// This is a workaround for macOS API limitations that do not return `AXUIElementRef` for windows on inactive spaces.
///
/// # Arguments
///
/// * `app` - A reference to the `Application` whose windows are to be brute-forced.
/// * `window_list` - A mutable vector of `WinID`s representing the expected global window list; found windows are removed from this list.
fn bruteforce_windows(app: &mut Application, window_list: &mut Vec<WinID>) -> Vec<Window> {
    const MAGIC: u32 = 0x636f_636f;
    let mut found_windows = Vec::new();
    debug!(
        "{}: App {:?} has unresolved window on other desktops, bruteforcing them.",
        function_name!(),
        app.psn(),
    );

    //
    // NOTE: MacOS API does not return AXUIElementRef of windows on inactive spaces. However,
    // we can just brute-force the element_id and create the AXUIElementRef ourselves.
    //  https://github.com/decodism
    //  https://github.com/lwouis/alt-tab-macos/issues/1324#issuecomment-2631035482
    //

    unsafe {
        const BUFSIZE: isize = 0x14;
        let Some(data_ref) = CFMutableData::new(None, BUFSIZE) else {
            error!("{}: error creating mutable data", function_name!());
            return found_windows;
        };
        CFMutableData::increase_length(data_ref.deref().into(), BUFSIZE);

        let data = from_raw_parts_mut(
            CFMutableData::mutable_byte_ptr(data_ref.deref().into()),
            BUFSIZE as usize,
        );
        let bytes = app.pid().to_ne_bytes();
        data[0x0..bytes.len()].copy_from_slice(&bytes);
        let bytes = MAGIC.to_ne_bytes();
        data[0x8..0x8 + bytes.len()].copy_from_slice(&bytes);

        for element_id in 0..0x7fffu64 {
            //
            // NOTE: Only the element_id changes between iterations.
            //

            let bytes = element_id.to_ne_bytes();
            data[0xc..0xc + bytes.len()].copy_from_slice(&bytes);

            let Ok(element_ref) =
                AXUIWrapper::retain(_AXUIElementCreateWithRemoteToken(data_ref.as_ref()))
            else {
                continue;
            };
            let Ok(window_id) = ax_window_id(element_ref.as_ptr()) else {
                continue;
            };

            if let Some(index) = window_list.iter().position(|&id| id == window_id) {
                window_list.remove(index);
                debug!("{}: Found window {window_id:?}", function_name!());
                if let Ok(window) = Window::new(&element_ref)
                    .inspect_err(|err| warn!("{}: {err}", function_name!()))
                {
                    found_windows.push(window);
                }
            }
        }
    }
    found_windows
}

/// Adds existing windows for a given application, attempting to resolve any that are not yet found.
/// It compares the application's reported window list with the global window list and uses brute-forcing if necessary.
///
/// # Arguments
///
/// * `cid` - The connection ID.
/// * `app` - A mutable reference to the `Application` whose windows are to be added.
/// * `spaces` - A slice of space IDs to query.
/// * `refresh_index` - An integer indicating the refresh index, used to determine if all windows are resolved.
/// * `windows` - A query for all windows.
///
/// # Returns
///
/// `Ok(Vec<Window>)` containing the found windows, otherwise `Err(Error)`.
fn add_existing_application_windows(
    cid: ConnID,
    app: &mut Application,
    spaces: &[u64],
    refresh_index: i32,
    windows: &Query<(&Window, Entity)>,
) -> Result<Vec<Window>> {
    let global_window_list = existing_application_window_list(cid, app, spaces, windows)?;
    if global_window_list.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "{}: No windows found for app {:?}",
                function_name!(),
                app.psn()
            ),
        ));
    }
    debug!(
        "{}: App {:?} has global windows: {global_window_list:?}",
        function_name!(),
        app.psn(),
    );

    let window_list = app.window_list();
    let window_count = window_list
        .as_ref()
        .map(|window_list| CFArray::count(window_list))
        .unwrap_or(0);

    let mut found_windows: Vec<Window> = Vec::new();
    let mut empty_count = 0;
    if let Ok(window_list) = window_list {
        for window_ref in get_array_values(&window_list) {
            let Ok(window_id) = ax_window_id(window_ref.as_ptr()) else {
                empty_count += 1;
                continue;
            };

            if !found_windows.iter().any(|window| window.id() == window_id) {
                let window_ref = AXUIWrapper::retain(window_ref.as_ptr())?;
                debug!(
                    "{}: Add window: {:?} {window_id}",
                    function_name!(),
                    app.psn()
                );
                if let Ok(window) = Window::new(&window_ref)
                    .inspect_err(|err| debug!("{}: {err}", function_name!()))
                {
                    found_windows.push(window);
                }
            }
        }
    }

    if isize::try_from(global_window_list.len())
        .is_ok_and(|length| length == (window_count - empty_count))
    {
        if refresh_index != -1 {
            debug!(
                "{}: All windows for {:?} are now resolved",
                function_name!(),
                app.psn(),
            );
        }
    } else {
        let find_window = |window_id| found_windows.iter().find(|window| window.id() == window_id);
        let mut app_window_list: Vec<WinID> = global_window_list
            .iter()
            .filter(|window_id| find_window(**window_id).is_none())
            .copied()
            .collect();

        if !app_window_list.is_empty() {
            debug!(
                "{}: {:?} has windows that are not yet resolved",
                function_name!(),
                app.psn(),
            );
            found_windows.extend(bruteforce_windows(app, &mut app_window_list));
        }
    }

    Ok(found_windows)
}

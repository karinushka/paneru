use bevy::app::Update;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::{ChildOf, Children};
use bevy::ecs::message::MessageWriter;
use bevy::ecs::query::{Has, Or, With};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::schedule::common_conditions::resource_equals;
use bevy::ecs::system::{Commands, Local, Populated, Query, Res};
use bevy::time::{Time, Virtual};
use core::ptr::NonNull;
use log::{debug, error, trace, warn};
use objc2_core_foundation::{
    CFArray, CFEqual, CFMutableData, CFNumber, CFNumberType, CFRetained, CFString, CGPoint, CGRect,
};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGError, CGGetActiveDisplayList, CGRectContainsPoint,
    CGWarpMouseCursorPosition,
};
use std::io::ErrorKind;
use std::ops::Deref;
use std::ptr::null_mut;
use std::slice::from_raw_parts_mut;
use std::time::Duration;
use stdext::function_name;

use crate::app::Application;
use crate::errors::{Error, Result};
use crate::events::{
    BProcess, Event, ExistingMarker, FreshMarker, OrphanedPane, PollForNotifications, SenderSocket,
    SpawnWindowTrigger, StrayFocusEvent, Timeout, Unmanaged, WMEventTrigger,
};
use crate::params::{ActiveDisplay, ActiveDisplayMut, ThrottledSystem};
use crate::platform::ProcessSerialNumber;
use crate::skylight::{
    _AXUIElementCreateWithRemoteToken, ConnID, SLSCopyActiveMenuBarDisplayIdentifier,
    SLSCopyAssociatedWindows, SLSCopyBestManagedDisplayForRect, SLSCopyManagedDisplayForWindow,
    SLSCopyManagedDisplaySpaces, SLSCopyWindowsWithOptionsAndTags, SLSFindWindowAndOwner,
    SLSGetConnectionIDForPSN, SLSGetCurrentCursorLocation, SLSGetWindowBounds, SLSMainConnectionID,
    SLSManagedDisplayGetCurrentSpace, SLSWindowIteratorAdvance, SLSWindowIteratorGetAttributes,
    SLSWindowIteratorGetParentID, SLSWindowIteratorGetTags, SLSWindowIteratorGetWindowID,
    SLSWindowQueryResultCopyWindows, SLSWindowQueryWindows, WinID,
};
use crate::util::{AXUIWrapper, create_array, get_array_values, get_cfdict_value};
use crate::windows::{Display, Window, ax_window_id};

/// The main window manager logic.
///
/// This struct contains the Bevy systems that respond to events and manage windows.
#[derive(Resource, Default)]
pub struct WindowManager {
    main_cid: ConnID,
}

impl WindowManager {
    /// Creates a new `WindowManager` instance.
    pub fn new() -> Self {
        let main_cid = unsafe { SLSMainConnectionID() };
        debug!("{}: My connection id: {main_cid}", function_name!());

        Self { main_cid }
    }

    /// Registers the Bevy systems for the `WindowManager`.
    ///
    /// # Arguments
    ///
    /// * `app` - The Bevy application to register the systems with.
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
        app.add_systems(
            Update,
            (
                WindowManager::display_changes_watcher,
                WindowManager::workspace_change_watcher,
            )
                .run_if(resource_equals(PollForNotifications(true))),
        );
    }

    /// Refreshes the list of active displays and reorganizes windows across them.
    /// It preserves spaces from old displays if they still exist.
    ///
    /// # Arguments
    ///
    /// * `display` - The display to refresh.
    /// * `windows` - A mutable query for all `Window` components.
    pub fn refresh_display(
        &self,
        display: &mut Display,
        windows: &mut Query<(&mut Window, Entity, Has<Unmanaged>)>,
    ) {
        debug!(
            "{}: Refreshing windows on display {}",
            function_name!(),
            display.id()
        );

        let display_bounds = display.bounds;
        for (space_id, pane) in &mut display.spaces {
            let mut lens = windows.transmute_lens::<(&Window, Entity)>();
            let new_windows = self.refresh_windows_space(*space_id, &lens.query());

            // Preserve the order - do not flush existing windows.
            for window_entity in pane.all_windows() {
                if !new_windows.contains(&window_entity) {
                    pane.remove(window_entity);
                }
            }
            for window_entity in new_windows {
                if windows
                    .get(window_entity)
                    .is_ok_and(|(_, _, unmanaged)| unmanaged)
                {
                    // Window is not managed, do not insert it into the panel.
                    continue;
                }
                if pane.index_of(window_entity).is_err() {
                    pane.append(window_entity);
                    if let Ok((mut window, _, _)) = windows.get_mut(window_entity) {
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
    /// * `wm` - The `WindowManager` resource.
    /// * `events` - The event sender socket resource.
    /// * `process_query` - A query for existing processes marked with `ExistingMarker`.
    /// * `commands` - Bevy commands to spawn entities and manage components.
    #[allow(clippy::needless_pass_by_value)]
    pub fn add_existing_process(
        wm: Res<WindowManager>,
        events: Res<SenderSocket>,
        process_query: Query<(Entity, &BProcess), With<ExistingMarker>>,
        mut commands: Commands,
    ) {
        for (entity, process) in process_query {
            let app = Application::new(&wm, &process.0, &events.0).unwrap();
            commands.spawn((app, ExistingMarker, ChildOf(entity)));
            commands.entity(entity).remove::<ExistingMarker>();
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
    pub fn add_existing_application(
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
                && let Ok(windows) = wm
                    .add_existing_application_windows(&mut app, &spaces, 0)
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
    /// * `wm` - The `WindowManager` resource.
    /// * `events` - The event sender socket resource.
    /// * `process_query` - A query for newly launched processes marked with `FreshMarker`.
    /// * `commands` - Bevy commands to spawn entities and manage components.
    #[allow(clippy::needless_pass_by_value)]
    pub fn add_launched_process(
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
                commands.entity(entity).remove::<FreshMarker>();
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

    /// A Bevy system that ticks timers and despawns entities when their timers finish.
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

    /// A Bevy system that retries focusing a window if the focus event arrived before the window was created.
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

    /// A Bevy system that finds and re-assigns orphaned spaces to the active display.
    #[allow(clippy::needless_pass_by_value)]
    pub fn find_orphaned_spaces(
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

    /// Returns child windows of the main window.
    pub fn get_associated_windows(&self, window_id: WinID) -> Vec<WinID> {
        let window_list = unsafe {
            let arr_ref = SLSCopyAssociatedWindows(self.main_cid, window_id);
            CFRetained::retain(arr_ref)
        };

        get_array_values(&window_list)
            .filter_map(|item| {
                let mut child_wid: WinID = 0;
                if unsafe {
                    CFNumber::value(
                        item.as_ref(),
                        CFNumberType::SInt32Type,
                        NonNull::from(&mut child_wid).as_ptr().cast(),
                    )
                } {
                    debug!(
                        "{}: checking {}'s childen: {}",
                        function_name!(),
                        window_id,
                        child_wid
                    );
                    (child_wid != 0).then_some(child_wid)
                } else {
                    warn!(
                        "{}: Unable to find subwindows of window {}: {item:?}.",
                        function_name!(),
                        window_id
                    );
                    None
                }
            })
            .collect()
    }

    /// Returns the connection ID for a given process serial number.
    pub fn connection_for_process(&self, psn: ProcessSerialNumber) -> Option<ConnID> {
        let mut connection: ConnID = 0;
        unsafe { SLSGetConnectionIDForPSN(self.main_cid, &psn, &mut connection) };
        (connection != 0).then_some(connection)
    }

    /// Retrieves a list of space IDs for a given display UUID.
    ///
    /// # Arguments
    ///
    /// * `uuid` - A reference to the `CFString` representing the display's UUID.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<u64>)` with the list of space IDs if successful, otherwise `Err(Error)`.
    fn display_space_list(&self, uuid: &CFString) -> Result<Vec<u64>> {
        let display_spaces = NonNull::new(unsafe { SLSCopyManagedDisplaySpaces(self.main_cid) })
            .map(|ptr| unsafe { CFRetained::from_raw(ptr) })
            .ok_or(Error::new(
                ErrorKind::PermissionDenied,
                format!(
                    "{}: can not copy managed display spaces for {}.",
                    function_name!(),
                    self.main_cid
                ),
            ))?;

        for display in get_array_values(display_spaces.as_ref()) {
            let display_ref = unsafe { display.as_ref() };
            trace!("{}: display {display_ref:?}", function_name!());
            let identifier = get_cfdict_value::<CFString>(
                display_ref,
                &CFString::from_static_str("Display Identifier"),
            )?;
            let identifier_ref = unsafe { identifier.as_ref() };
            debug!(
                "{}: identifier {identifier_ref:?} uuid {uuid:?}",
                function_name!()
            );
            // FIXME: For some reason the main display does not have a UUID in the name, but is
            // referenced as simply "Main".
            if identifier_ref.to_string().ne("Main") && !CFEqual(Some(identifier_ref), Some(uuid)) {
                continue;
            }

            let spaces =
                get_cfdict_value::<CFArray>(display_ref, &CFString::from_static_str("Spaces"))?;
            debug!("{}: spaces {spaces:?}", function_name!());

            let space_list = get_array_values(unsafe { spaces.as_ref() })
                .filter_map(|space| {
                    let num = get_cfdict_value::<CFNumber>(
                        unsafe { space.as_ref() },
                        &CFString::from_static_str("id64"),
                    )
                    .ok()?;

                    let mut id = 0u64;
                    unsafe {
                        CFNumber::value(
                            num.as_ref(),
                            CFNumber::r#type(num.as_ref()),
                            NonNull::from(&mut id).as_ptr().cast(),
                        )
                    };
                    (id != 0).then_some(id)
                })
                .collect::<Vec<u64>>();
            return Ok(space_list);
        }
        Err(Error::new(
            ErrorKind::NotFound,
            format!(
                "{}: could not get any displays for {}",
                function_name!(),
                self.main_cid
            ),
        ))
    }

    /// Retrieves a list of all currently present displays, along with their associated spaces.
    ///
    /// # Returns
    ///
    /// A `Vec<Self>` containing `Display` objects for all present displays.
    pub fn present_displays(&self) -> Vec<Display> {
        let mut count = 0u32;
        unsafe {
            CGGetActiveDisplayList(0, null_mut(), &raw mut count);
        }
        if count < 1 {
            return vec![];
        }
        let mut displays = Vec::with_capacity(count.try_into().unwrap());
        unsafe {
            CGGetActiveDisplayList(count, displays.as_mut_ptr(), &raw mut count);
            displays.set_len(count.try_into().unwrap());
        }
        displays
            .into_iter()
            .flat_map(|id| {
                let uuid = Display::uuid_from_id(id);
                uuid.and_then(|uuid| {
                    self.display_space_list(uuid.as_ref())
                        .map(|spaces| Display::new(id, spaces))
                })
            })
            .collect()
    }

    /// Retrieves the UUID of the active menu bar display.
    ///
    /// # Returns
    ///
    /// `Ok(CFRetained<CFString>)` with the UUID if successful, otherwise `Err(Error)`.
    fn active_display_uuid(&self) -> Result<CFRetained<CFString>> {
        unsafe {
            let ptr = SLSCopyActiveMenuBarDisplayIdentifier(self.main_cid);
            let ptr = NonNull::new(ptr.cast_mut()).ok_or(Error::new(
                ErrorKind::NotFound,
                format!(
                    "{}: can not find active display for connection {}.",
                    function_name!(),
                    self.main_cid
                ),
            ))?;
            Ok(CFRetained::from_raw(ptr))
        }
    }

    /// Retrieves the `CGDirectDisplayID` of the active menu bar display.
    ///
    /// # Returns
    ///
    /// `Ok(u32)` with the display ID if successful, otherwise `Err(Error)`.
    pub fn active_display_id(&self) -> Result<u32> {
        let uuid = self.active_display_uuid()?;
        Display::id_from_uuid(&uuid)
    }

    /// Retrieves the ID of the current active space on this display.
    ///
    /// # Returns
    ///
    /// `Ok(u64)` with the space ID if successful, otherwise `Err(Error)`.
    pub fn active_display_space(&self, display_id: CGDirectDisplayID) -> Result<u64> {
        // let cid = self.main_cid;
        // let uuid = Display::active_display_uuid(cid);
        // uuid.map(|uuid| unsafe { SLSManagedDisplayGetCurrentSpace(cid, uuid.deref()) })
        Display::uuid_from_id(display_id).map(|uuid| unsafe {
            SLSManagedDisplayGetCurrentSpace(self.main_cid, &raw const *uuid)
        })
    }

    /// Retrieves the UUID of the display the window is currently on.
    /// It first tries `SLSCopyManagedDisplayForWindow` and then falls back to `SLSCopyBestManagedDisplayForRect`.
    ///
    /// # Returns
    ///
    /// `Ok(Retained<CFString>)` with the display UUID if successful, otherwise `Err(Error)`.
    fn display_uuid(&self, window_id: WinID) -> Result<CFRetained<CFString>> {
        let uuid = unsafe {
            NonNull::new(SLSCopyManagedDisplayForWindow(self.main_cid, window_id).cast_mut())
                .map(|uuid| CFRetained::from_raw(uuid))
        };
        uuid.or_else(|| {
            let mut frame = CGRect::default();
            unsafe {
                SLSGetWindowBounds(self.main_cid, window_id, &mut frame);
                NonNull::new(SLSCopyBestManagedDisplayForRect(self.main_cid, frame).cast_mut())
                    .map(|uuid| CFRetained::from_raw(uuid))
            }
        })
        .ok_or(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "{}: can not get display uuid for window {window_id}.",
                function_name!()
            ),
        ))
    }

    /// Retrieves the `CGDirectDisplayID` of the display the window is currently on.
    ///
    /// # Returns
    ///
    /// `Ok(u32)` with the display ID if successful, otherwise `Err(Error)`.
    fn display_id(&self, window_id: WinID) -> Result<CGDirectDisplayID> {
        let uuid = self.display_uuid(window_id);
        uuid.and_then(|uuid| Display::id_from_uuid(&uuid))
    }

    /// Centers the mouse cursor on the window if it's not already within the window's bounds.
    pub fn center_mouse(&self, window: &Window, display_bounds: &CGRect) {
        let mut cursor = CGPoint::default();
        if unsafe { CGError::Success != SLSGetCurrentCursorLocation(self.main_cid, &mut cursor) } {
            warn!(
                "{}: Unable to get current cursor position.",
                function_name!()
            );
            return;
        }
        let frame = window.frame();
        if CGRectContainsPoint(frame, cursor) {
            return;
        }

        let center = CGPoint::new(
            display_bounds.origin.x + frame.origin.x + frame.size.width / 2.0,
            display_bounds.origin.y + frame.origin.y + frame.size.height / 2.0,
        );
        let display_id = self.display_id(window.id());
        #[allow(clippy::redundant_closure)]
        let bounds = display_id.map(|display_id| CGDisplayBounds(display_id));
        if bounds.is_ok_and(|bounds| !CGRectContainsPoint(bounds, center)) {
            return;
        }
        CGWarpMouseCursorPosition(center);
    }

    /// Adds existing windows for a given application, attempting to resolve any that are not yet found.
    /// It compares the application's reported window list with the global window list and uses brute-forcing if necessary.
    ///
    /// # Arguments
    ///
    /// * `app` - A mutable reference to the `Application` whose windows are to be added.
    /// * `spaces` - A slice of space IDs to query.
    /// * `refresh_index` - An integer indicating the refresh index, used to determine if all windows are resolved.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<Window>)` containing the found windows, otherwise `Err(Error)`.
    // bool window_manager_add_existing_application_windows(struct space_manager *sm,
    // struct window_manager *wm, struct application *application, int refresh_index)
    fn add_existing_application_windows(
        &self,
        app: &mut Application,
        spaces: &[u64],
        refresh_index: i32,
    ) -> Result<Vec<Window>> {
        // uint32_t *global_window_list = window_manager_existing_application_window_list(application, &global_window_count);
        // if (!global_window_list) return result;
        let global_window_list = existing_application_window_list(self.main_cid, app, spaces)?;
        if global_window_list.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("{}: No windows found for {app}", function_name!()),
            ));
        }
        debug!(
            "{}: {app} has global windows: {global_window_list:?}",
            function_name!()
        );

        // CFArrayRef window_list_ref = application_window_list(application);
        // int window_count = window_list_ref ? CFArrayGetCount(window_list_ref) : 0;
        let window_list = app.window_list();
        let window_count = window_list
            .as_ref()
            .map(|window_list| CFArray::count(window_list))
            .unwrap_or(0);

        let mut found_windows: Vec<Window> = Vec::new();
        let mut empty_count = 0;
        if let Ok(window_list) = window_list {
            // for idx in 0..window_count {
            for window_ref in get_array_values(&window_list) {
                let Ok(window_id) = ax_window_id(window_ref.as_ptr()) else {
                    empty_count += 1;
                    continue;
                };

                //
                // FIXME: The AX API appears to always include a single element for Finder that
                // returns an empty window id. This is likely the desktop window. Other similar
                // cases should be handled the same way; simply ignore the window when we attempt
                // to do an equality check to see if we have correctly discovered the number of
                // windows to track.
                //

                // if (!window_manager_find_window(wm, window_id)) {
                //     window_manager_create_and_add_window(
                //     sm, wm, application, CFRetain(window_ref), window_id, false);
                // }
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

        // if (global_window_count == window_count-empty_count)
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
            // for (int i = 0; i < global_window_count; ++i) {
            //     struct window *window = window_manager_find_window(wm, global_window_list[i]);
            //     if (!window) {
            //         missing_window = true;
            //         break;
            //     }
            // }
            let find_window =
                |window_id| found_windows.iter().find(|window| window.id() == window_id);
            let mut app_window_list: Vec<WinID> = global_window_list
                .iter()
                .filter(|window_id| find_window(**window_id).is_none())
                .copied()
                .collect();

            // if (missing_window) {
            if !app_window_list.is_empty() {
                debug!(
                    "{}: {:?} has windows that are not yet resolved",
                    function_name!(),
                    app.psn(),
                );
                found_windows.extend(bruteforce_windows(app, &mut app_window_list));

                // } else {
                //     // debug("%s: all windows for %s are now resolved\n", __FUNCTION__, application->name);
                //     info!(
                //         "add_existing_application_windows: All windows for {} are now resolved (second pass)",
                //         app.inner().name
                //     );
                //     // buf_del(wm->applications_to_refresh, refresh_index);
                //     result = true;
            }
        }

        // if (window_list_ref) CFRelease(window_list_ref);
        Ok(found_windows)
    }

    /// Repopulates the current window panel with eligible windows from a specified space.
    ///
    /// # Arguments
    ///
    /// * `space_id` - The ID of the space to refresh windows from.
    /// * `windows` - A query for all windows.
    fn refresh_windows_space(
        &self,
        space_id: u64,
        windows: &Query<(&Window, Entity)>,
    ) -> Vec<Entity> {
        space_window_list_for_connection(self.main_cid, &[space_id], None, false)
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

    /// Finds a window at a given screen point using `SkyLight` API.
    ///
    /// # Arguments
    ///
    /// * `point` - A reference to the `CGPoint` representing the screen coordinate.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` with the found window's ID if successful, otherwise `Err(Error)`.
    pub fn find_window_at_point(&self, point: &CGPoint) -> Result<WinID> {
        let mut window_id: WinID = 0;
        let mut window_conn_id: ConnID = 0;
        let mut window_point = CGPoint { x: 0f64, y: 0f64 };
        unsafe {
            SLSFindWindowAndOwner(
                self.main_cid,
                0, // filter window id
                1,
                0,
                point,
                &mut window_point,
                &mut window_id,
                &mut window_conn_id,
            )
        };
        if self.main_cid == window_conn_id {
            unsafe {
                SLSFindWindowAndOwner(
                    self.main_cid,
                    window_id,
                    -1,
                    0,
                    point,
                    &mut window_point,
                    &mut window_id,
                    &mut window_conn_id,
                )
            };
        }
        if window_id == 0 {
            Err(Error::invalid_window(&format!(
                "{}: could not find a window at {point:?}",
                function_name!()
            )))
        } else {
            Ok(window_id)
        }
    }

    /// Returns a list of windows in a given workspace.
    pub fn windows_in_workspace(&self, space_id: u64) -> Result<Vec<WinID>> {
        space_window_list_for_connection(self.main_cid, &[space_id], None, true)
    }

    /// Periodically checks for windows moved between spaces and displays.
    /// TODO: Workaround for Tahoe 26.x, where workspace notifications are not arriving. So if a
    /// window is missing in the current space, try to trigger a workspace change event.
    #[allow(clippy::needless_pass_by_value)]
    pub fn workspace_change_watcher(
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

    /// Periodically checks for displays added and removed.
    /// TODO: Workaround for Tahoe 26.x, where display change notifications are not arriving.
    #[allow(clippy::needless_pass_by_value)]
    pub fn display_changes_watcher(
        active_display: ActiveDisplay,
        // displays: Query<&Display, Without<ActiveDisplayMarker>>,
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
        if current_display_id == active_display.id() {
            return;
        }

        let present_displays = window_manager.present_displays();
        active_display.displays().for_each(|display| {
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

        if active_display
            .displays()
            .any(|display| display.id() == current_display_id)
        {
            debug!(
                "{}: detected dislay change from {}.",
                function_name!(),
                active_display.id(),
            );
            commands.trigger(WMEventTrigger(Event::DisplayChanged));
            return;
        }

        debug!(
            "{}: detected added display {}.",
            function_name!(),
            current_display_id
        );
        commands.trigger(WMEventTrigger(Event::DisplayAdded {
            display_id: current_display_id,
        }));
    }
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
///
/// # Returns
///
/// `Ok(Vec<WinID>)` containing the list of window IDs if successful, otherwise `Err(Error)`.
fn space_window_list_for_connection(
    main_cid: ConnID,
    spaces: &[u64],
    cid: Option<ConnID>,
    also_minimized: bool,
) -> Result<Vec<WinID>> {
    let space_list_ref = create_array(spaces, CFNumberType::SInt64Type)?;

    let mut set_tags = 0i64;
    let mut clear_tags = 0i64;
    let options = if also_minimized { 0x7 } else { 0x2 };
    let ptr = NonNull::new(unsafe {
        SLSCopyWindowsWithOptionsAndTags(
            main_cid,
            cid.unwrap_or(0),
            &raw const *space_list_ref,
            options,
            &mut set_tags,
            &mut clear_tags,
        )
    })
    .ok_or(Error::new(
        ErrorKind::InvalidInput,
        format!(
            "{}: nullptr returned from SLSCopyWindowsWithOptionsAndTags.",
            function_name!()
        ),
    ))?;
    let window_list_ref = unsafe { CFRetained::from_raw(ptr) };

    let count = window_list_ref.count();
    if count == 0 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("{}: zero windows returned", function_name!()),
        ));
    }

    let query = unsafe {
        CFRetained::from_raw(SLSWindowQueryWindows(
            main_cid,
            &raw const *window_list_ref,
            count,
        ))
    };
    let iterator =
        unsafe { CFRetained::from_raw(SLSWindowQueryResultCopyWindows(query.deref().into())) };

    let mut window_list = Vec::with_capacity(count.try_into().unwrap());
    while unsafe { SLSWindowIteratorAdvance(&raw const *iterator) } {
        let tags = unsafe { SLSWindowIteratorGetTags(&raw const *iterator) };
        let attributes = unsafe { SLSWindowIteratorGetAttributes(&raw const *iterator) };
        let parent_wid: WinID = unsafe { SLSWindowIteratorGetParentID(&raw const *iterator) };
        let window_id: WinID = unsafe { SLSWindowIteratorGetWindowID(&raw const *iterator) };

        trace!(
            "{}: id: {window_id} parent: {parent_wid} tags: 0x{tags:x} attributes: 0x{attributes:x}",
            function_name!()
        );
        if found_valid_window(parent_wid, attributes, tags) {
            window_list.push(window_id);
        }
    }
    Ok(window_list)
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
) -> Result<Vec<WinID>> {
    if spaces.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("{}: no spaces returned", function_name!()),
        ));
    }
    space_window_list_for_connection(cid, spaces, app.connection(), true)
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
        "{}: {app} has unresolved window on other desktops, bruteforcing them.",
        function_name!()
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

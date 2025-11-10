use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::{ChildOf, Children};
use bevy::ecs::observer::On;
use bevy::ecs::query::{With, Without};
use bevy::ecs::system::{Commands, Query, Res, ResMut};
use bevy::time::{Time, Virtual};
use bevy::transform::commands::BuildChildrenTransformExt;
use core::ptr::NonNull;
use log::{debug, error, trace, warn};
use objc2_core_foundation::{
    CFArray, CFMutableData, CFNumber, CFNumberType, CFRetained, CGPoint, CGRect,
};
use std::collections::HashMap;
use std::io::{Error, ErrorKind, Result};
use std::mem::take;
use std::ops::Deref;
use std::slice::from_raw_parts_mut;
use stdext::function_name;

use crate::app::Application;
use crate::config::Config;
use crate::events::{
    BProcess, Event, ExistingMarker, FocusFollowsMouse, FocusedMarker, FreshMarker, MainConnection,
    MissionControlActive, OrphanedSpaces, RepositionMarker, ReshuffleAroundTrigger, SenderSocket,
    SkipReshuffle, WMEventTrigger,
};
use crate::platform::ProcessSerialNumber;
use crate::process::Process;
use crate::skylight::{
    _AXUIElementCreateWithRemoteToken, _SLPSGetFrontProcess, ConnID, SLSCopyAssociatedWindows,
    SLSCopyWindowsWithOptionsAndTags, SLSFindWindowAndOwner, SLSWindowIteratorAdvance,
    SLSWindowIteratorGetAttributes, SLSWindowIteratorGetParentID, SLSWindowIteratorGetTags,
    SLSWindowIteratorGetWindowID, SLSWindowQueryResultCopyWindows, SLSWindowQueryWindows, WinID,
};
use crate::util::{AXUIWrapper, create_array, get_array_values};
use crate::windows::{Display, Panel, Window, WindowPane, ax_window_id, ax_window_pid};

const THRESHOLD: f64 = 10.0;

/// The main window manager logic.
///
/// This struct contains the Bevy systems that respond to events and manage windows.
#[derive(Default)]
pub struct WindowManager;

impl WindowManager {
    /// Dispatches process-related messages, such as application launch and termination.
    ///
    /// # Arguments
    ///
    /// * `trigger` - The Bevy event trigger containing the application event.
    /// * `processes` - A query for all processes.
    /// * `commands` - Bevy commands to spawn or despawn entities.
    #[allow(clippy::needless_pass_by_value)]
    pub fn application_event_trigger(
        trigger: On<WMEventTrigger>,
        processes: Query<(&BProcess, Entity)>,
        mut commands: Commands,
    ) {
        let find_process = |psn| {
            processes
                .iter()
                .find(|(BProcess(process), _)| &process.psn == psn)
        };

        match &trigger.event().0 {
            Event::ApplicationLaunched { psn, observer } => {
                if find_process(psn).is_none() {
                    let process = Process::new(psn, observer.clone());
                    commands.spawn((FreshMarker, BProcess(process)));
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
    /// * `displays` - A query for all displays.
    /// * `main_cid` - The main connection ID resource.
    /// * `commands` - Bevy commands to spawn or despawn entities.
    #[allow(clippy::needless_pass_by_value)]
    pub fn dispatch_application_messages(
        trigger: On<WMEventTrigger>,
        windows: Query<(&Window, Entity)>,
        mut displays: Query<&mut Display, With<FocusedMarker>>,
        main_cid: Res<MainConnection>,
        mut commands: Commands,
    ) {
        let main_cid = main_cid.0;
        let find_window = |window_id| windows.iter().find(|(window, _)| window.id() == window_id);
        let Ok(mut active_display) = displays.single_mut() else {
            warn!("{}: Unable to get current display.", function_name!());
            return;
        };

        match &trigger.event().0 {
            Event::WindowCreated { element } => match Window::new(element) {
                Ok(window) => {
                    commands.spawn((FreshMarker, window));
                }
                Err(err) => debug!(
                    "{}: not adding window {element:?}: {}",
                    function_name!(),
                    err
                ),
            },
            Event::WindowMinimized { window_id } => {
                if let Some((_, entity)) = find_window(*window_id) {
                    active_display.remove_window(entity);
                }
            }
            Event::WindowDeminimized { window_id } => {
                let Ok(pane) = active_display.active_panel(main_cid) else {
                    return;
                };
                if let Some((_, entity)) = find_window(*window_id) {
                    pane.append(entity);
                }
            }
            _ => (),
        }
    }

    /// Handles Mission Control events, updating the `MissionControlActive` resource.
    ///
    /// # Arguments
    ///
    /// * `trigger` - The Bevy event trigger containing the Mission Control event.
    /// * `mission_control_active` - The `MissionControlActive` resource.
    #[allow(clippy::needless_pass_by_value)]
    pub fn mission_control_trigger(
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
    pub fn swipe_gesture_trigger(
        trigger: On<WMEventTrigger>,
        active_display: Query<&Display, With<FocusedMarker>>,
        mut focused_window: Query<(&mut Window, Entity), With<FocusedMarker>>,
        main_cid: Res<MainConnection>,
        config: Option<Res<Config>>,
        mut commands: Commands,
    ) {
        const SWIPE_THRESHOLD: f64 = 0.01;
        let Event::Swipe { ref deltas } = trigger.event().0 else {
            return;
        };
        if config
            .and_then(|config| config.options().swipe_gesture_fingers)
            .is_some_and(|fingers| deltas.len() == fingers)
        {
            let Ok(active_display) = active_display.single() else {
                warn!("{}: Unable to get current display.", function_name!());
                return;
            };
            let delta = deltas.iter().sum::<f64>();
            if delta.abs() > SWIPE_THRESHOLD {
                WindowManager::slide_window(
                    main_cid.0,
                    &mut focused_window,
                    active_display,
                    delta,
                    &mut commands,
                );
            }
        }
    }

    /// Finds and re-inserts orphaned spaces into displays that have empty spaces.
    ///
    /// # Arguments
    ///
    /// * `orphaned_spaces` - A map of space IDs to `WindowPane`s that are currently orphaned.
    /// * `displays` - A query for all displays.
    /// * `windows` - A query for all windows.
    fn find_orphaned_spaces(
        orphaned_spaces: &mut HashMap<u64, WindowPane>,
        display: &mut Display,
        windows: &mut Query<&mut Window>,
    ) {
        let mut relocated_windows = vec![];
        for (space_id, pane) in &mut display.spaces {
            debug!(
                "{}: Checking space {space_id} for orphans: {pane}",
                function_name!()
            );
            if let Some(space) = orphaned_spaces.remove(space_id) {
                debug!(
                    "{}: Reinserting orphaned space {space_id} into display {}",
                    function_name!(),
                    display.id
                );
                for window_id in space.all_windows() {
                    // TODO: check for clashing windows.
                    pane.append(window_id);
                    relocated_windows.push((window_id, display.bounds));
                }
            }
        }

        for (entity, bounds) in relocated_windows {
            let Ok(mut window) = windows.get_mut(entity) else {
                continue;
            };
            let ratio = window.width_ratio;
            debug!(
                "{}: Resizing relocated window {} to ratio {ratio:.02}",
                function_name!(),
                window.id()
            );
            window.resize(bounds.size.width * ratio, bounds.size.height, &bounds);
        }
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
            let new_windows =
                WindowManager::refresh_windows_space(main_cid, *space_id, &lens.query());

            // Preserve the order - do not flush existing windows.
            for window_entity in pane.all_windows() {
                if !new_windows.contains(&window_entity) {
                    pane.remove(window_entity);
                }
            }
            for window_entity in new_windows {
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

    /// Repopulates the current window panel with eligible windows from a specified space.
    ///
    /// # Arguments
    ///
    /// * `main_cid` - The main connection ID.
    /// * `space_id` - The ID of the space to refresh windows from.
    /// * `find_window` - A closure to find a window by its ID.
    fn refresh_windows_space(
        main_cid: ConnID,
        space_id: u64,
        windows: &Query<(&Window, Entity)>,
    ) -> Vec<Entity> {
        WindowManager::space_window_list_for_connection(main_cid, &[space_id], None, false, windows)
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

    /// Sets the currently focused window based on the frontmost application.
    ///
    /// # Arguments
    ///
    /// * `trigger` - The Bevy event trigger for the focus check.
    /// * `windows` - A query for all windows.
    /// * `focused_window` - A query for the currently focused window.
    /// * `commands` - Bevy commands to trigger events and manage components.
    #[allow(clippy::needless_pass_by_value)]
    pub fn currently_focused_trigger(
        trigger: On<WMEventTrigger>,
        windows: Query<(&Window, Entity)>,
        focused_window: Query<(&Window, Entity), With<FocusedMarker>>,
        mut commands: Commands,
    ) {
        if !matches!(trigger.event().0, Event::CurrentlyFocused) {
            return;
        }

        debug!("{}: {} windows.", function_name!(), windows.iter().len());
        let mut focused_psn = ProcessSerialNumber::default();
        unsafe {
            _SLPSGetFrontProcess(&mut focused_psn);
        }
        let Some((window, entity)) = windows
            .iter()
            .find(|(window, _)| window.psn().as_ref().is_some_and(|psn| &focused_psn == psn))
        else {
            warn!(
                "{}: Unable to set currently focused window.",
                function_name!()
            );
            return;
        };

        if let Ok((previous, prev_entity)) = focused_window.single() {
            if previous.id() == window.id() {
                return;
            }
            commands.entity(prev_entity).remove::<FocusedMarker>();
        }
        commands.entity(entity).insert(FocusedMarker);
        commands.trigger(ReshuffleAroundTrigger(window.id()));
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
    /// * `find_window` - A closure to find a window by its ID.
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
            let iterator =
                CFRetained::from_raw(SLSWindowQueryResultCopyWindows(query.deref().into()));

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
                        if also_minimized || !window.is_minimized() {
                            window_list.push(window.id());
                        }
                    }
                    None => {
                        if WindowManager::found_valid_window(parent_wid, attributes, tags) {
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
    /// * `find_window` - A closure to find a window by its ID.
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
        WindowManager::space_window_list_for_connection(
            cid,
            spaces,
            app.connection(),
            true,
            windows,
        )
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
        let global_window_list =
            WindowManager::existing_application_window_list(cid, app, spaces, windows)?;
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

                //
                // FIXME: The AX API appears to always include a single element for Finder that
                // returns an empty window id. This is likely the desktop window. Other similar
                // cases should be handled the same way; simply ignore the window when we attempt
                // to do an equality check to see if we have correctly discovered the number of
                // windows to track.
                //

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
            let find_window =
                |window_id| found_windows.iter().find(|window| window.id() == window_id);
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
                found_windows.extend(WindowManager::bruteforce_windows(app, &mut app_window_list));
            }
        }

        Ok(found_windows)
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
    pub fn front_switched_trigger(
        trigger: On<WMEventTrigger>,
        processes: Query<(&BProcess, &Children)>,
        applications: Query<&Application>,
        focused_window: Query<(&Window, Entity), With<FocusedMarker>>,
        mut focus_follows_mouse_id: ResMut<FocusFollowsMouse>,
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
                "{}: Multiple apps registered to process {}.",
                function_name!(),
                process.name
            );
        }
        let Some(app) = children
            .first()
            .and_then(|entity| applications.get(*entity).ok())
        else {
            error!(
                "{}: No application for process {}.",
                function_name!(),
                process.name
            );
            return;
        };
        debug!("{}: {}", function_name!(), process.name);

        match app.focused_window_id() {
            Err(_) => {
                let Ok((_, focused_entity)) = focused_window.single() else {
                    warn!("{}: window_manager_set_window_opacity", function_name!());
                    return;
                };

                focus_follows_mouse_id.as_mut().0 = None;
                commands.entity(focused_entity).remove::<FocusedMarker>();
                warn!("{}: reset focused window", function_name!());
            }
            Ok(focused_id) => commands.trigger(WMEventTrigger(Event::WindowFocused {
                window_id: focused_id,
            })),
        }
    }

    /// Handles the event when a new window is created. It adds the window to the manager and sets focus.
    ///
    /// # Arguments
    ///
    /// * `windows` - A query for newly created windows marked with `FreshMarker`.
    /// * `focused_window` - A query for the currently focused window.
    /// * `apps` - A query for all applications.
    /// * `active_display` - A query for the active display.
    /// * `main_cid` - The main connection ID resource.
    /// * `commands` - Bevy commands to manage components and trigger events.
    #[allow(clippy::needless_pass_by_value)]
    pub fn window_create(
        windows: Query<(Entity, &mut Window), With<FreshMarker>>,
        focused_window: Query<(Entity, &Window), (With<FocusedMarker>, Without<FreshMarker>)>,
        mut apps: Query<(Entity, &mut Application)>,
        mut active_display: Query<&mut Display, With<FocusedMarker>>,
        main_cid: Res<MainConnection>,
        mut commands: Commands,
    ) {
        for (entity, mut window) in windows {
            commands.entity(entity).remove::<FreshMarker>();
            debug!(
                "{}: window {} entity {}",
                function_name!(),
                window.id(),
                entity
            );
            let Ok(pid) = ax_window_pid(&window.element()) else {
                warn!(
                    "{}: Unable to get window pid for {}",
                    function_name!(),
                    window.id()
                );
                return;
            };
            let Some((app_entity, mut app)) = apps.iter_mut().find(|(_, app)| app.pid() == pid)
            else {
                warn!(
                    "{}: unable to find application with {pid}.",
                    function_name!()
                );
                return;
            };

            debug!(
                "{}: created {} title: {} role: {} subrole: {} element: {}",
                function_name!(),
                window.id(),
                window.title().unwrap_or_default(),
                window.role().unwrap_or_default(),
                window.subrole().unwrap_or_default(),
                window.element(),
            );
            commands.entity(entity).set_parent_in_place(app_entity);

            if app.observe_window(&window).is_err() {
                warn!(
                    "{}: Error observing window {}.",
                    function_name!(),
                    window.id()
                );
            }

            window.psn = Some(app.psn());
            let minimized = window.is_minimized();
            let is_root = Window::parent(app.connection().unwrap_or_default(), window.id())
                .is_err()
                || window.is_root();
            {
                window.minimized = minimized;
                window.is_root = is_root;
            }
            debug!(
                "{}: window {} isroot {} eligible {}",
                function_name!(),
                window.id(),
                window.is_root(),
                window.is_eligible(),
            );

            let Ok(mut active_display) = active_display.single_mut() else {
                return;
            };
            debug!("Active display {}", active_display.id);
            _ = window.update_frame(Some(&active_display.bounds));

            let Ok(panel) = active_display.active_panel(main_cid.0) else {
                return;
            };

            let insert_at = focused_window
                .single()
                .ok()
                .and_then(|(entity, _)| panel.index_of(entity).ok());
            debug!("New window adding at {panel}");
            match insert_at {
                Some(after) => {
                    debug!("New window inserted at {after}");
                    _ = panel.insert_at(after, entity);
                }
                None => panel.append(entity),
            }
            commands.trigger(WMEventTrigger(Event::WindowFocused {
                window_id: window.id(),
            }));
        }
    }

    /// Handles the event when a window is destroyed. It removes the window from the ECS world and relevant displays.
    ///
    /// # Arguments
    ///
    /// * `windows` - A query for windows marked with `DestroyedMarker`.
    /// * `focused_window` - A query for the currently focused window.
    /// * `apps` - A query for all applications.
    /// * `displays` - A query for all displays.
    /// * `commands` - Bevy commands to despawn entities and trigger events.
    #[allow(clippy::needless_pass_by_value)]
    pub fn window_destroyed_trigger(
        trigger: On<WMEventTrigger>,
        windows: Query<(&Window, Entity, &ChildOf)>,
        mut apps: Query<&mut Application>,
        mut displays: Query<&mut Display>,
        main_cid: Res<MainConnection>,
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

        for mut display in &mut displays {
            let Ok(panel) = display.active_panel(main_cid.0) else {
                continue;
            };
            let mut previous_id = None;
            _ = panel.access_left_of(entity, |panel| {
                previous_id = panel
                    .top()
                    .and_then(|entity| windows.get(entity).ok())
                    .map(|tuple| tuple.0.id());
                false
            });
            display.remove_window(entity);
            if let Some(previous_id) = previous_id {
                commands.trigger(ReshuffleAroundTrigger(previous_id));
            }
        }
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
    pub fn window_resized_trigger(
        trigger: On<WMEventTrigger>,
        mut windows: Query<(&mut Window, Entity)>,
        displays: Query<&mut Display, With<FocusedMarker>>,
        mut commands: Commands,
    ) {
        let Event::WindowResized { window_id } = trigger.event().0 else {
            return;
        };
        let Ok(active_display) = displays.single() else {
            warn!("{}: Unable to get current display.", function_name!());
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

    /// Handles the event when a window gains focus. It updates the focused window, PSN, and reshuffles windows.
    /// It also centers the mouse on the focused window if focus-follows-mouse is enabled.
    ///
    /// # Arguments
    ///
    /// * `trigger` - The Bevy event trigger containing the window focused event.
    /// * `applications` - A query for all applications.
    /// * `windows` - A query for all windows with their parent and focus state.
    /// * `current_focus` - A query for the currently focused window.
    /// * `main_cid` - The main connection ID resource.
    /// * `focus_follows_mouse_id` - The resource to track focus follows mouse window ID.
    /// * `skip_reshuffle` - The resource to indicate if reshuffling should be skipped.
    /// * `commands` - Bevy commands to manage components and trigger events.
    #[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
    pub fn window_focused_trigger(
        trigger: On<WMEventTrigger>,
        applications: Query<&Application>,
        windows: Query<(&Window, Entity, &ChildOf, Option<&FocusedMarker>)>,
        current_focus: Query<(&Window, Entity), With<FocusedMarker>>,
        initializing: Query<&InitializingMarker>,
        main_cid: Res<MainConnection>,
        mut focus_follows_mouse_id: ResMut<FocusFollowsMouse>,
        mut skip_reshuffle: ResMut<SkipReshuffle>,
        mut commands: Commands,
    ) {
        let Event::WindowFocused { window_id } = trigger.event().0 else {
            return;
        };
        if initializing.single().is_ok() {
            return;
        }
        let main_cid = main_cid.0;
        let Some((window, entity, child)) =
            windows.iter().find_map(|(window, entity, child, _)| {
                (window.id() == window_id).then_some((window, entity, child))
            })
        else {
            error!("{}: Unable to find window id {window_id}", function_name!());
            return;
        };
        for (window, entity, _, focused) in windows {
            if focused.is_some() && window.id() != window_id {
                commands.entity(entity).remove::<FocusedMarker>();
            }
            if focused.is_none() && window.id() == window_id {
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

        let previous_focus = current_focus.single().ok();
        if let Some((_, previous_entity)) = previous_focus {
            commands.entity(previous_entity).remove::<FocusedMarker>();
        }
        if previous_focus.is_none_or(|(previous, _)| previous.id() != window_id)
            && focus_follows_mouse_id.0.is_none_or(|id| id != window_id)
        {
            window.center_mouse(main_cid);
        }

        commands.entity(entity).insert(FocusedMarker);
        focus_follows_mouse_id.as_mut().0 = None;

        if skip_reshuffle.0 {
            skip_reshuffle.as_mut().0 = false;
        } else {
            commands.trigger(ReshuffleAroundTrigger(window.id()));
        }
    }

    /// Reshuffles windows around the given `window` within the active panel to ensure visibility.
    /// Windows to the right and left of the focused window are repositioned.
    ///
    /// # Arguments
    ///
    /// * `trigger` - The Bevy event trigger containing the ID of the window to reshuffle around.
    /// * `main_cid` - The main connection ID resource.
    /// * `active_display` - A query for the active display.
    /// * `windows` - A query for all windows.
    #[allow(clippy::needless_pass_by_value)]
    pub fn reshuffle_around(
        main_cid: ConnID,
        active_display: &mut Query<&mut Display, With<FocusedMarker>>,
        entity: Entity,
        windows: &mut Query<(&mut Window, Entity, Option<&RepositionMarker>)>,
        commands: &mut Commands,
    ) -> Result<()> {
        let mut active_display = active_display.single_mut().map_err(|err| {
            Error::new(
                ErrorKind::NotFound,
                format!("{}: active display not found: {err}", function_name!()),
            )
        })?;
        let display_bounds = active_display.bounds;
        let active_panel = active_display.active_panel(main_cid)?;

        let (window, _, moving) = windows.get_mut(entity).map_err(|err| {
            Error::new(
                ErrorKind::NotFound,
                format!("{}: window not found: {err}", function_name!()),
            )
        })?;
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

        WindowManager::reposition_stack(
            frame.origin.x,
            &panel,
            frame.size.width,
            &display_bounds,
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
                if upper_left > display_bounds.size.width - THRESHOLD {
                    upper_left = display_bounds.size.width - THRESHOLD;
                }

                if (frame.origin.x - upper_left).abs() > 0.1 {
                    WindowManager::reposition_stack(
                        upper_left,
                        panel,
                        frame.size.width,
                        &display_bounds,
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
                if upper_left < THRESHOLD {
                    upper_left = THRESHOLD;
                }
                upper_left -= frame.size.width;

                if (frame.origin.x - upper_left).abs() > 0.1 {
                    WindowManager::reposition_stack(
                        upper_left,
                        panel,
                        frame.size.width,
                        &display_bounds,
                        windows,
                        commands,
                    );
                }
            }
            true // continue through all windows
        });
        Ok(())
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn reshuffle_around_trigger(
        trigger: On<ReshuffleAroundTrigger>,
        main_cid: Res<MainConnection>,
        mut active_display: Query<&mut Display, With<FocusedMarker>>,
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
            _ = WindowManager::reshuffle_around(
                main_cid.0,
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

    /// Repositions all windows within a given panel stack.
    ///
    /// # Arguments
    ///
    /// * `upper_left` - The x-coordinate of the upper-left corner of the stack.
    /// * `panel` - The panel containing the windows to reposition.
    /// * `width` - The width of each window in the stack.
    /// * `display_bounds` - The bounds of the display.
    /// * `find_window` - A closure to find a window by its ID.
    fn reposition_stack(
        upper_left: f64,
        panel: &Panel,
        width: f64,
        display_bounds: &CGRect,
        windows: &mut Query<(&mut Window, Entity, Option<&RepositionMarker>)>,
        commands: &mut Commands,
    ) {
        const REMAINING_THERSHOLD: f64 = 200.0;
        let display_height = display_bounds.size.height;
        let entities = match panel {
            Panel::Single(entity) => vec![*entity],
            Panel::Stack(stack) => stack.clone(),
        };
        let count: f64 = u32::try_from(entities.len()).unwrap().into();
        let mut fits = 0f64;
        let mut height = 0f64;
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
            commands
                .spawn((app, ExistingMarker))
                .set_parent_in_place(entity);
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
                && let Ok(windows) = WindowManager::add_existing_application_windows(
                    cid.0, &mut app, &spaces, 0, &windows,
                )
                .inspect_err(|err| warn!("{}: {err}", function_name!()))
            {
                for window in windows {
                    debug!(
                        "adding found windows: {} {}",
                        window.id(),
                        window.title().unwrap_or_default()
                    );
                    commands
                        .spawn((window, FreshMarker))
                        .set_parent_in_place(entity);
                }
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
        process_query: Query<(Entity, &mut BProcess, Option<&Children>), With<FreshMarker>>,
        time: Res<Time<Virtual>>,
        mut commands: Commands,
    ) {
        for (entity, mut process, children) in process_query {
            process.0.ready_timer.tick(time.delta());

            if process.0.terminated {
                commands.entity(entity).despawn();
                continue;
            }
            if !process.0.ready() {
                trace!(
                    "{}: Timer {}",
                    function_name!(),
                    process.0.ready_timer.elapsed().as_secs_f32()
                );
                if process.0.ready_timer.is_finished() {
                    debug!(
                        "{}: app {} is still not observable. Removing",
                        function_name!(),
                        process.0.name
                    );
                    process.0.terminated = true;
                }
                continue;
            }

            //
            // NOTE: If we somehow receive a duplicate launched event due to the
            // subscription-timing-mess above, simply ignore the event..
            //
            if children.is_some() {
                commands.entity(entity).remove::<FreshMarker>();
                continue;
            }

            let mut app = Application::new(cid.0, &process.0, &events.0).unwrap();

            if app.observe().is_ok_and(|good| good) {
                commands
                    .spawn((app, FreshMarker))
                    .set_parent_in_place(entity);
            } else {
                error!(
                    "{}: failed to register some observers {}",
                    function_name!(),
                    process.0.name
                );
            }

            debug!(
                "{}: app {} ready after {}ms.",
                function_name!(),
                process.0.name,
                process.0.ready_timer.elapsed().as_millis(),
            );
            commands.entity(entity).remove::<FreshMarker>();
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
        app_query: Query<(&mut Application, Entity), With<FreshMarker>>,
        windows: Query<&Window>,
        mut commands: Commands,
    ) {
        // TODO: maybe refactor this with add_existing_application_windows()
        let find_window = |window_id| windows.iter().find(|window| window.id() == window_id);

        for (app, entity) in app_query {
            let array = app.window_list().unwrap();
            let create_window = |element_ref: NonNull<_>| {
                let element = AXUIWrapper::retain(element_ref.as_ptr());
                element.map(|element| {
                    let window_id = ax_window_id(element.as_ptr())
                        .inspect_err(|err| {
                            warn!("{}: error adding window: {err}", function_name!());
                        })
                        .ok()?;
                    find_window(window_id).map_or_else(
                        // Window does not exist, create it.
                        || {
                            Window::new(&element)
                                .inspect_err(|err| {
                                    warn!("{}: error adding window: {err}.", function_name!());
                                })
                                .ok()
                        },
                        // Window already exists, skip it.
                        |_| None,
                    )
                })
            };
            let windows = get_array_values::<accessibility_sys::__AXUIElement>(&array)
                .flat_map(create_window)
                .flatten()
                .collect::<Vec<_>>();
            for window in windows {
                commands
                    .spawn((window, FreshMarker))
                    .set_parent_in_place(entity);
            }
            commands.entity(entity).remove::<FreshMarker>();
        }
    }

    /// Checks if the "focus follows mouse" feature is enabled.
    ///
    /// # Arguments
    ///
    /// * `config` - An optional reference to the `Config` resource.
    ///
    /// # Returns
    ///
    /// `true` if focus follows mouse is enabled, `false` otherwise.
    pub fn focus_follows_mouse(config: Option<&Res<Config>>) -> bool {
        config
            .and_then(|config| config.options().focus_follows_mouse)
            .is_some_and(|ffm| ffm)
    }

    /// Handles display added or removed events.
    /// It updates the list of displays and re-evaluates orphaned spaces.
    ///
    /// # Arguments
    ///
    /// * `trigger` - The Bevy event trigger containing the display event.
    /// * `displays` - A query for all displays.
    /// * `windows` - A query for all windows.
    /// * `main_cid` - The main connection ID resource.
    /// * `commands` - Bevy commands to spawn/despawn entities and trigger events.
    #[allow(clippy::needless_pass_by_value)]
    pub fn display_add_remove_trigger(
        trigger: On<WMEventTrigger>,
        mut displays: Query<(&mut Display, Entity)>,
        mut windows: Query<&mut Window>,
        main_cid: Res<MainConnection>,
        mut orphaned_spaces: ResMut<OrphanedSpaces>,
        mut commands: Commands,
    ) {
        let main_cid = main_cid.0;
        let orphaned_spaces = &mut orphaned_spaces.0;
        match trigger.event().0 {
            Event::DisplayAdded { display_id } => {
                debug!("{}: Display Added: {display_id:?}", function_name!());
                let Some(mut display) = Display::present_displays(main_cid)
                    .into_iter()
                    .find(|display| display.id == display_id)
                else {
                    error!("{}: Unable to find added display!", function_name!());
                    return;
                };
                WindowManager::find_orphaned_spaces(orphaned_spaces, &mut display, &mut windows);

                for (id, pane) in &display.spaces {
                    debug!("{}: Space {id} - {pane}", function_name!());
                }
                commands.spawn(display);
                commands.trigger(WMEventTrigger(Event::DisplayChanged));
            }

            Event::DisplayRemoved { display_id } => {
                debug!("{}: Display Removed: {display_id:?}", function_name!());
                let Some((mut display, entity)) = displays
                    .iter_mut()
                    .find(|(display, _)| display.id == display_id)
                else {
                    error!("{}: Unable to find removed display!", function_name!());
                    return;
                };
                for (space_id, pane) in take(&mut display.spaces)
                    .into_iter()
                    .filter(|(_, pane)| pane.len() > 0)
                {
                    debug!("{}: adding {pane} to orphaned list.", function_name!(),);
                    orphaned_spaces.insert(space_id, pane);
                }

                for (mut display, _) in displays {
                    WindowManager::find_orphaned_spaces(
                        orphaned_spaces,
                        &mut display,
                        &mut windows,
                    );
                }
                commands.entity(entity).despawn();
                commands.trigger(WMEventTrigger(Event::DisplayChanged));
            }

            Event::DisplayMoved { display_id } => {
                debug!("{}: Display Moved: {display_id:?}", function_name!());
                let Some((mut display, _)) = displays
                    .iter_mut()
                    .find(|(display, _)| display.id == display_id)
                else {
                    error!("{}: Unable to find moved display!", function_name!());
                    return;
                };
                let Some(moved_display) = Display::present_displays(main_cid)
                    .into_iter()
                    .find(|display| display.id == display_id)
                else {
                    return;
                };
                *display = moved_display;
                WindowManager::find_orphaned_spaces(orphaned_spaces, &mut display, &mut windows);

                for (id, pane) in &display.spaces {
                    debug!("{}: Space {id} - {pane}", function_name!());
                }
                commands.trigger(WMEventTrigger(Event::DisplayChanged));
            }

            _ => (),
        }
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
    pub fn display_change_trigger(
        trigger: On<WMEventTrigger>,
        focused_window: Query<(&Window, Entity), With<FocusedMarker>>,
        mut displays: Query<(&mut Display, Entity, Option<&FocusedMarker>)>,
        main_cid: Res<MainConnection>,
        mut commands: Commands,
    ) {
        if !matches!(trigger.event().0, Event::DisplayChanged) {
            // Maybe also react to Event::SpaceChanged.
            return;
        }

        let main_cid = main_cid.0;
        let Ok(active_id) = Display::active_display_id(main_cid) else {
            error!("{}: Unable to get active display id!", function_name!());
            return;
        };

        if let Some((previous_display, previous_entity, _)) =
            displays.iter().find(|(_, _, focused)| focused.is_some())
        {
            if previous_display.id == active_id {
                return;
            }
            commands.entity(previous_entity).remove::<FocusedMarker>();
        }

        _ = WindowManager::display_change(
            active_id,
            main_cid,
            &mut displays,
            &focused_window,
            &mut commands,
        )
        .inspect_err(|err| warn!("{}: {err}", function_name!()));
    }

    fn display_change(
        active_id: u32,
        main_cid: ConnID,
        displays: &mut Query<(&mut Display, Entity, Option<&FocusedMarker>)>,
        focused_window: &Query<(&Window, Entity), With<FocusedMarker>>,
        commands: &mut Commands,
    ) -> Result<()> {
        let (mut active_display, entity, _) = displays
            .iter_mut()
            .find(|(display, _, _)| display.id == active_id)
            .ok_or(Error::new(
                ErrorKind::NotFound,
                "Can not find active display {display_id}.",
            ))?;
        commands.entity(entity).insert(FocusedMarker);
        debug!(
            "{}: Display ({active_id}) or Workspace changed, reorienting windows.",
            function_name!(),
        );

        let (window, entity) = focused_window.single().map_err(|err| {
            Error::new(
                ErrorKind::NotFound,
                format!("Can not find active windows: {err}."),
            )
        })?;
        let panel = active_display.active_panel(main_cid)?;
        debug!("{}: Active panel {panel}", function_name!());

        if !window.managed() || panel.index_of(entity).is_ok() {
            return Ok(());
        }
        debug!(
            "{}: Window {} moved between displays or workspaces.",
            function_name!(),
            window.id(),
        );

        // Current window is not present in the current pane. This is probably due to it being
        // moved to a different desktop. Re-insert it into a correct pane.
        for (mut display, _, _) in displays {
            // First remove it from all the displays.
            display.remove_window(entity);

            if display.id == active_id {
                // .. and then re-insert it into the current one.
                if let Ok(panel) = display.active_panel(main_cid) {
                    panel.append(entity);
                }
            }
        }

        commands.trigger(ReshuffleAroundTrigger(window.id()));
        Ok(())
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
        main_cid: ConnID,
        focused_window: &mut Query<(&mut Window, Entity), With<FocusedMarker>>,
        active_display: &Display,
        delta_x: f64,
        commands: &mut Commands,
    ) {
        trace!("{}: Windows slide {delta_x}.", function_name!());
        let Ok((mut window, _)) = focused_window.single_mut() else {
            warn!("{}: No window focused.", function_name!());
            return;
        };
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
        window.center_mouse(main_cid);
        commands.trigger(ReshuffleAroundTrigger(window.id()));
    }

    /// Finds a window at a given screen point using `SkyLight` API.
    ///
    /// # Arguments
    ///
    /// * `main_cid` - The main connection ID.
    /// * `point` - A reference to the `CGPoint` representing the screen coordinate.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` with the found window's ID if successful, otherwise `Err(Error)`.
    fn find_window_at_point(main_cid: ConnID, point: &CGPoint) -> Result<WinID> {
        let mut window_id: WinID = 0;
        let mut window_conn_id: ConnID = 0;
        let mut window_point = CGPoint { x: 0f64, y: 0f64 };
        unsafe {
            SLSFindWindowAndOwner(
                main_cid,
                0, // filter window id
                1,
                0,
                point,
                &mut window_point,
                &mut window_id,
                &mut window_conn_id,
            )
        };
        if main_cid == window_conn_id {
            unsafe {
                SLSFindWindowAndOwner(
                    main_cid,
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
            Err(Error::other(format!(
                "{}: could not find a window at {point:?}",
                function_name!()
            )))
        } else {
            Ok(window_id)
        }
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
    /// * `mission_control_active` - A resource indicating if Mission Control is active.
    /// * `focus_follows_mouse_id` - A resource to track the window ID for focus-follows-mouse.
    /// * `skip_reshuffle` - A resource to indicate if reshuffling should be skipped.
    #[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
    pub fn mouse_moved_trigger(
        trigger: On<WMEventTrigger>,
        windows: Query<&Window>,
        focused_window: Query<(&Window, Entity), With<FocusedMarker>>,
        main_cid: Res<MainConnection>,
        config: Option<Res<Config>>,
        mission_control_active: Res<MissionControlActive>,
        mut focus_follows_mouse_id: ResMut<FocusFollowsMouse>,
        mut skip_reshuffle: ResMut<SkipReshuffle>,
    ) {
        let Event::MouseMoved { point } = trigger.event().0 else {
            return;
        };
        let main_cid = main_cid.0;

        if !WindowManager::focus_follows_mouse(config.as_ref()) {
            return;
        }
        if mission_control_active.0 {
            return;
        }
        if focus_follows_mouse_id.0.is_some() {
            trace!("{}: ffm_window_id > 0", function_name!());
            return;
        }
        let Ok(window_id) = WindowManager::find_window_at_point(main_cid, &point) else {
            // TODO: notfound
            return;
        };
        let Ok((focused_window, _)) = focused_window.single() else {
            // TODO: notfound
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

        let window_list = unsafe {
            let arr_ref = SLSCopyAssociatedWindows(main_cid, window_id);
            CFRetained::retain(arr_ref)
        };

        let mut window = window;
        for item in get_array_values(&window_list) {
            let mut child_wid: WinID = 0;
            unsafe {
                if !CFNumber::value(
                    item.as_ref(),
                    CFNumberType::SInt32Type,
                    NonNull::from(&mut child_wid).as_ptr().cast(),
                ) {
                    warn!(
                        "{}: Unable to find subwindows of window {}: {item:?}.",
                        function_name!(),
                        window_id
                    );
                    continue;
                }
            };
            debug!(
                "{}: checking {}'s childen: {}",
                function_name!(),
                window_id,
                child_wid
            );
            let Some(child_window) = windows.iter().find(|window| window.id() == child_wid) else {
                warn!(
                    "{}: Unable to find child window {child_wid}.",
                    function_name!()
                );
                continue;
            };

            let Ok(role) = window.role() else {
                warn!("{}: finding role for {window_id}", function_name!(),);
                continue;
            };

            let valid = ["AXSheet", "AXDrawer"]
                .iter()
                .any(|axrole| axrole.eq(&role));

            if valid {
                window = child_window;
                break;
            }
        }

        //  Do not reshuffle windows due to moved mouse focus.
        skip_reshuffle.as_mut().0 = true;
        window.focus_without_raise(focused_window);
        focus_follows_mouse_id.as_mut().0 = Some(window_id);
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
    pub fn mouse_down_trigger(
        trigger: On<WMEventTrigger>,
        windows: Query<&Window>,
        active_display: Query<&Display, With<FocusedMarker>>,
        main_cid: Res<MainConnection>,
        mission_control_active: Res<MissionControlActive>,
        mut commands: Commands,
    ) {
        let Event::MouseDown { point } = trigger.event().0 else {
            return;
        };
        debug!("{}: {point:?}", function_name!());
        if mission_control_active.0 {
            return;
        }
        let main_cid = main_cid.0;
        let Ok(active_display) = active_display.single() else {
            warn!("{}: Unable to get current display.", function_name!());
            return;
        };

        let Some(window) = WindowManager::find_window_at_point(main_cid, &point)
            .ok()
            .and_then(|window_id| windows.iter().find(|window| window.id() == window_id))
        else {
            return;
        };
        if !window.fully_visible(&active_display.bounds) {
            // WindowManager::reshuffle_around(main_cid, &window, active_display, find_window)?;
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
    pub fn mouse_dragged_trigger(
        trigger: On<WMEventTrigger>,
        mission_control_active: Res<MissionControlActive>,
    ) {
        let Event::MouseDragged { point } = trigger.event().0 else {
            return;
        };
        trace!("{}: {point:?}", function_name!());

        if mission_control_active.0 {
            #[warn(clippy::needless_return)]
            return;
        }
    }
}

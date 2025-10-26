use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::{ChildOf, Children};
use bevy::ecs::observer::On;
use bevy::ecs::query::{QuerySingleError, With, Without};
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
use std::ops::Deref;
use std::slice::from_raw_parts_mut;
use stdext::function_name;
use stdext::prelude::RwLockExt;

use crate::app::Application;
use crate::config::Config;
use crate::events::{
    ApplicationTrigger, BProcess, DisplayChangeTrigger, Event, ExistingMarker, FocusedMarker,
    FreshMarker, FrontSwitchedMarker, MainConnection, MouseTrigger, SenderSocket,
    WindowManagerResource,
};
use crate::platform::ProcessSerialNumber;
use crate::process::Process;
use crate::skylight::{
    _AXUIElementCreateWithRemoteToken, _SLPSGetFrontProcess, ConnID, SLSCopyAssociatedWindows,
    SLSCopyWindowsWithOptionsAndTags, SLSFindWindowAndOwner, SLSWindowIteratorAdvance,
    SLSWindowIteratorGetAttributes, SLSWindowIteratorGetParentID, SLSWindowIteratorGetTags,
    SLSWindowIteratorGetWindowID, SLSWindowQueryResultCopyWindows, SLSWindowQueryWindows, WinID,
};
use crate::util::{AxuWrapperType, create_array, get_array_values};
use crate::windows::{Display, Panel, Window, WindowPane, ax_window_id, ax_window_pid};

const THRESHOLD: f64 = 10.0;

pub struct WindowManager {
    main_cid: ConnID,
    last_window: Option<WinID>, // TODO: use this for "goto last window bind"
    pub focused_window: Option<WinID>,
    pub focused_psn: ProcessSerialNumber,
    pub ffm_window_id: Option<WinID>,
    mission_control_is_active: bool,
    pub skip_reshuffle: bool,
    focus_follows_mouse: bool,
    swipe_gesture_fingers: Option<usize>,
    orphaned_spaces: HashMap<u64, WindowPane>,
    mouse_down_window: Option<Window>,
    down_location: CGPoint,
}

impl WindowManager {
    /// Creates a new `WindowManager` instance.
    ///
    /// # Arguments
    ///
    /// * `events` - An `EventSender` to send events to the event handler.
    /// * `main_cid` - The main connection ID for the `SkyLight` API.
    ///
    /// # Returns
    ///
    /// A new `WindowManager` instance.
    pub fn new(main_cid: ConnID) -> Self {
        WindowManager {
            main_cid,
            last_window: None,
            focused_window: None,
            focused_psn: ProcessSerialNumber::default(),
            ffm_window_id: None,
            mission_control_is_active: false,
            skip_reshuffle: false,
            focus_follows_mouse: true,
            swipe_gesture_fingers: None,
            orphaned_spaces: HashMap::new(),
            mouse_down_window: None,
            down_location: CGPoint::default(),
        }
    }

    #[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
    pub fn application_trigger(
        trigger: On<ApplicationTrigger>,
        processes: Query<(&BProcess, Entity)>,
        windows: Query<(&Window, Entity)>,
        displays: Query<&mut Display, With<FocusedMarker>>,
        mut window_manager: ResMut<WindowManagerResource>,
        mut commands: Commands,
    ) {
        let window_manager = &mut window_manager.0;
        let find_process = |psn| {
            processes
                .iter()
                .find(|(BProcess(process), _)| &process.psn == psn)
        };
        let find_entity = |window_id| windows.iter().find(|(window, _)| window.id() == window_id);
        let find_window = |window_id| find_entity(window_id).map(|(window, _)| window).cloned();

        let Ok(active_display) = displays.single() else {
            warn!("{}: Unable to get current display.", function_name!());
            return;
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
            Event::ApplicationFrontSwitched { psn } => {
                if let Some((_, entity)) = find_process(psn) {
                    commands.entity(entity).insert(FrontSwitchedMarker);
                }
            }

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
            Event::WindowDestroyed { window_id } => {
                if let Some((_, entity)) = find_entity(*window_id) {
                    displays
                        .iter()
                        .for_each(|display| display.remove_window(*window_id));
                    commands.entity(entity).despawn();
                    let previous = window_manager.focused_window.and_then(find_window);
                    if let Some(window) = previous {
                        _ = window_manager.reshuffle_around(&window, active_display, &find_window);
                    }
                }
            }
            Event::WindowFocused { window_id } => {
                if let Some((_, entity)) = find_entity(*window_id) {
                    commands.entity(entity).insert(FocusedMarker);
                }
            }
            Event::CurrentlyFocused => {
                window_manager.currently_focused(
                    &windows.iter().map(|(window, _)| window).collect::<Vec<_>>(),
                    active_display,
                    &find_window,
                );
            }

            Event::WindowMoved { window_id } => window_manager.window_moved(*window_id),
            Event::WindowResized { window_id } => {
                let Some(window) = find_window(*window_id) else {
                    return;
                };
                _ = window_manager.window_resized(&window, active_display, &find_window);
            }

            Event::Swipe { deltas } => {
                const SWIPE_THRESHOLD: f64 = 0.01;
                if window_manager
                    .swipe_gesture_fingers
                    .is_some_and(|fingers| deltas.len() == fingers)
                {
                    let delta = deltas.iter().sum::<f64>();
                    if delta.abs() > SWIPE_THRESHOLD {
                        _ = window_manager.slide_window(active_display, delta, &find_window);
                    }
                }
            }

            Event::MissionControlShowAllWindows
            | Event::MissionControlShowFrontWindows
            | Event::MissionControlShowDesktop => {
                window_manager.mission_control_is_active = true;
            }
            Event::MissionControlExit => {
                window_manager.mission_control_is_active = false;
            }

            Event::WindowMinimized { window_id } => {
                active_display.remove_window(*window_id);
            }
            Event::WindowDeminimized { window_id } => {
                let Ok(pane) = active_display.active_panel(window_manager.main_cid) else {
                    return;
                };
                pane.append(*window_id);
            }

            Event::ConfigRefresh { config } => {
                window_manager.reload_config(config);
            }

            event => {
                debug!("Unhandled event {event:?}");
            }
        }
    }

    /// Reloads the manager's configuration based on the provided `Config` object.
    ///
    /// # Arguments
    ///
    /// * `config` - The new `Config` object to load.
    fn reload_config(&mut self, config: &Config) {
        debug!("{}: Got fresh config: {config:?}", function_name!());
        self.focus_follows_mouse = config
            .options()
            .focus_follows_mouse
            .is_some_and(|focus| focus);
        self.swipe_gesture_fingers = config.options().swipe_gesture_fingers;
    }

    fn find_orphaned_spaces<F: Fn(WinID) -> Option<Window>>(
        &mut self,
        displays: Vec<&mut Display>,
        find_window: &F,
    ) {
        let mut relocated_windows = vec![];

        for display in displays {
            let spaces = display
                .spaces
                .iter()
                // Only consider spaces which have no registered windows.
                .filter_map(|(space_id, window_pane)| (window_pane.len() == 0).then_some(*space_id))
                .collect::<Vec<_>>();
            debug!(
                "{}: Attempting to relocate into empty spaces: {spaces:?}",
                function_name!()
            );

            for space_id in spaces {
                if let Some(space) = self.orphaned_spaces.remove(&space_id) {
                    debug!(
                        "{}: Reinserted orphand space {space_id} into display {}",
                        function_name!(),
                        display.id
                    );
                    for window_id in space.all_windows() {
                        relocated_windows.push((window_id, display.bounds));
                    }
                    display.spaces.insert(space_id, space);
                }
            }
        }

        relocated_windows
            .iter()
            .filter_map(|(window_id, bounds)| find_window(*window_id).zip(Some(bounds)))
            .for_each(|(window, bounds)| {
                let ratio = window.inner().width_ratio;
                debug!(
                    "{}: Resizing relocated window {} to ratio {ratio:.02}",
                    function_name!(),
                    window.id()
                );
                window.resize(bounds.size.width * ratio, bounds.size.height, bounds);
            });
    }

    /// Refreshes the list of active displays and reorganizes windows across them.
    /// It preserves spaces from old displays if they still exist.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the displays are refreshed successfully, otherwise `Err(Error)`.
    pub fn refresh_displays<F: Fn(WinID) -> Option<Window>>(
        main_cid: ConnID,
        displays: &mut Vec<&Display>,
        find_window: &F,
    ) {
        // let displays = Display::present_displays(main_cid);
        // if displays.is_empty() {
        //     return Err(Error::new(
        //         ErrorKind::NotFound,
        //         format!("{}: Can not find any displays?!", function_name!()),
        //     ));
        // }

        for display in displays {
            let display_bounds = display.bounds;
            for (space_id, pane) in &display.spaces {
                pane.clear();
                let windows =
                    WindowManager::refresh_windows_space(main_cid, *space_id, find_window);
                debug!("{}: space {space_id}:", function_name!());
                for window in &windows {
                    debug!(
                        "{}: window {}:",
                        function_name!(),
                        window.title().unwrap_or_default()
                    );
                }

                // .for_each(|window| {
                //     displays
                //         .iter()
                //         .for_each(|display| display.remove_window(window.inner().id));
                //     pane.append(window.id());
                // });
                windows
                    .iter()
                    .map(super::windows::Window::id)
                    .for_each(|window_id| pane.append(window_id));

                pane.all_windows()
                    .iter()
                    .filter_map(|window_id| find_window(*window_id))
                    .for_each(|window| {
                        _ = window.update_frame(Some(&display_bounds));
                    });
            }
        }
    }

    /// Repopulates the current window panel with eligible windows from a specified space.
    ///
    /// # Arguments
    ///
    /// * `space_id` - The ID of the space to refresh windows from.
    /// * `pane` - A reference to the `WindowPane` to which windows will be appended.
    fn refresh_windows_space<F: Fn(WinID) -> Option<Window>>(
        main_cid: ConnID,
        space_id: u64,
        find_window: &F,
    ) -> Vec<Window> {
        WindowManager::space_window_list_for_connection(
            main_cid,
            &[space_id],
            None,
            false,
            find_window,
        )
        .inspect_err(|err| {
            warn!(
                "{}: getting windows for space {space_id}: {err}",
                function_name!()
            );
        })
        .unwrap_or_default()
        .into_iter()
        .filter_map(find_window)
        .filter(Window::is_eligible)
        .collect()
    }

    pub fn currently_focused<F: Fn(WinID) -> Option<Window>>(
        &mut self,
        windows: &[&Window],
        active_display: &Display,
        find_window: &F,
    ) {
        debug!("{}: {} windows.", function_name!(), windows.len());
        let mut focused_psn = ProcessSerialNumber::default();
        unsafe {
            _SLPSGetFrontProcess(&mut focused_psn);
        }
        let Some(window) = windows.iter().find(|window| {
            window
                .inner()
                .psn
                .as_ref()
                .is_some_and(|psn| &focused_psn == psn)
        }) else {
            warn!(
                "{}: Unable to set currently focused window.",
                function_name!()
            );
            return;
        };
        self.last_window = Some(window.id());
        self.focused_window = Some(window.id());
        self.focused_psn = focused_psn;
        _ = self.reshuffle_around(window, active_display, &find_window);
    }

    /// Checks if Mission Control is currently active.
    ///
    /// # Returns
    ///
    /// `true` if Mission Control is active, `false` otherwise.
    pub fn mission_control_is_active(&self) -> bool {
        self.mission_control_is_active
    }

    /// Retrieves a list of window IDs for specified spaces and connection, with an option to include minimized windows.
    /// This function uses `SkyLight` API calls.
    ///
    /// # Arguments
    ///
    /// * `spaces` - A vector of space IDs to query windows from.
    /// * `cid` - An optional connection ID. If `None`, the main connection ID is used.
    /// * `also_minimized` - A boolean indicating whether to include minimized windows in the result.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<WinID>)` containing the list of window IDs if successful, otherwise `Err(Error)`.
    fn space_window_list_for_connection<F: Fn(WinID) -> Option<Window>>(
        main_cid: ConnID,
        spaces: &[u64],
        cid: Option<ConnID>,
        also_minimized: bool,
        find_window: &F,
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
                let wid: WinID = SLSWindowIteratorGetWindowID(&raw const *iterator);

                trace!(
                    "{}: id: {wid} parent: {parent_wid} tags: 0x{tags:x} attributes: 0x{attributes:x}",
                    function_name!()
                );
                match find_window(wid) {
                    Some(window) => {
                        if also_minimized || !window.is_minimized() {
                            window_list.push(window.id());
                        }
                    }
                    None => {
                        if WindowManager::found_valid_window(parent_wid, attributes, tags) {
                            window_list.push(wid);
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
    /// * `app` - A reference to the `Application` for which to retrieve window IDs.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<WinID>)` containing the list of window IDs if successful, otherwise `Err(Error)`.
    fn existing_application_window_list<F: Fn(WinID) -> Option<Window>>(
        cid: ConnID,
        app: &Application,
        spaces: &[u64],
        find_window: &F,
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
            find_window,
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
            let bytes = app.pid().unwrap().to_ne_bytes();
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
                    AxuWrapperType::retain(_AXUIElementCreateWithRemoteToken(data_ref.as_ref()))
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
    /// * `app` - A reference to the `Application` whose windows are to be added.
    /// * `refresh_index` - An integer indicating the refresh index, used to determine if all windows are resolved.
    ///
    /// # Returns
    ///
    /// `Ok(bool)` where `true` indicates all windows were resolved, otherwise `Err(Error)`.
    fn add_existing_application_windows(
        cid: ConnID,
        app: &mut Application,
        spaces: &[u64],
        refresh_index: i32,
    ) -> Result<Vec<Window>> {
        let global_window_list =
            WindowManager::existing_application_window_list(cid, app, spaces, &|_| None)?;
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
                    let window_ref = AxuWrapperType::retain(window_ref.as_ptr())?;
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

    /// Creates a new `Window` object and adds it to the application's managed windows.
    /// It performs checks for unknown or non-real windows and observes the window for events.
    ///
    /// # Arguments
    ///
    /// * `app` - A reference to the `Application` that owns the window.
    /// * `window_ref` - A `CFRetained<AxuWrapperType>` reference to the Accessibility UI element of the window.
    /// * `window_id` - The ID of the window.
    /// * `_one_shot_rules` - A boolean flag (currently unused, TODO: fix).
    ///
    /// # Returns
    ///
    /// `Ok(Window)` if the window is created and added successfully, otherwise `Err(Error)`.
    // struct window *window_manager_create_and_add_window(
    // struct space_manager *sm, struct window_manager *wm, struct application *application,
    // AXUIElementRef window_ref, uint32_t window_id, bool one_shot_rules)
    fn create_managed_window(window_ref: &CFRetained<AxuWrapperType>) -> Result<Window> {
        let window = Window::new(window_ref)?;
        if window.is_unknown() {
            return Err(Error::other(format!(
                "{}: Ignoring AXUnknown window, id: {}",
                function_name!(),
                window.id()
            )));
        }

        if !window.is_real() {
            return Err(Error::other(format!(
                "{}: Ignoring non-real window, id: {}",
                function_name!(),
                window.id()
            )));
        }

        debug!(
            "{}: created {} title: {} role: {} subrole: {}",
            function_name!(),
            window.id(),
            window.title().unwrap_or_default(),
            window.role().unwrap_or_default(),
            window.subrole().unwrap_or_default(),
        );
        Ok(window)
    }

    /// Handles the event when an application switches to the front. It updates the focused window and PSN.
    ///
    /// # Arguments
    ///
    /// * `psn` - A reference to the `ProcessSerialNumber` of the application that switched to front.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the front switch is processed successfully, otherwise `Err(Error)`.
    pub fn front_switched(
        processes: Query<(&BProcess, Entity, &Children), With<FrontSwitchedMarker>>,
        applications: Query<&Application>,
        windows: Query<(&Window, Entity)>,
        mut window_manager: ResMut<WindowManagerResource>,
        mut commands: Commands,
    ) {
        for (BProcess(process), entity, children) in processes {
            commands.entity(entity).remove::<FrontSwitchedMarker>();
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
                continue;
            };
            debug!("{}: {}", function_name!(), process.name);

            let window_manager = &mut window_manager.0;
            let find_window = |window_id| {
                windows
                    .iter()
                    .find_map(|(window, _)| (window.id() == window_id).then_some(window))
            };
            match app.focused_window_id() {
                Err(_) => {
                    let focused_window = window_manager.focused_window.and_then(find_window);
                    if focused_window.is_none() {
                        warn!("{}: window_manager_set_window_opacity", function_name!());
                    }

                    window_manager.last_window = window_manager.focused_window;
                    window_manager.focused_window = None;
                    window_manager.focused_psn = process.psn.clone();
                    window_manager.ffm_window_id = None;
                    warn!("{}: reset focused window", function_name!());
                }
                Ok(focused_id) => {
                    let Some((_, focused_entity)) =
                        windows.iter().find(|(window, _)| window.id() == focused_id)
                    else {
                        return;
                    };
                    commands.entity(focused_entity).insert(FocusedMarker);
                }
            }
        }
    }

    /// Handles the event when a new window is created. It adds the window to the manager and sets focus.
    ///
    /// # Arguments
    ///
    /// * `ax_element` - A `CFRetained<AxuWrapperType>` reference to the Accessibility UI element of the new window.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the window is created successfully, otherwise `Err(Error)`.
    #[allow(clippy::needless_pass_by_value)]
    pub fn window_create(
        windows: Query<(Entity, &mut Window), With<FreshMarker>>,
        focused_window: Query<(Entity, &Window), (With<FocusedMarker>, Without<FreshMarker>)>,
        apps: Query<(Entity, &mut Application)>,
        displays: Query<&Display, With<FocusedMarker>>,
        main_cid: Res<MainConnection>,
        mut commands: Commands,
    ) {
        for (entity, window) in windows {
            commands.entity(entity).remove::<FreshMarker>();
            debug!(
                "{}: window {} entity {}",
                function_name!(),
                window.id(),
                entity
            );
            let element = window.element();
            let Ok(window_id) = ax_window_id(element.as_ptr()) else {
                warn!(
                    "{}: Unable to get window id for {element:?}",
                    function_name!()
                );
                return;
            };
            let Ok(pid) = ax_window_pid(&element) else {
                warn!(
                    "{}: Unable to get window pid for {window_id}",
                    function_name!()
                );
                return;
            };
            let Some((app_entity, app)) = apps.iter().find(|(_, app)| app.pid().unwrap() == pid)
            else {
                warn!(
                    "{}: unable to find application with {pid}.",
                    function_name!()
                );
                return;
            };

            debug!(
                "{}: created {} title: {} role: {} subrole: {} element: {:x?}",
                function_name!(),
                window.id(),
                window.title().unwrap_or_default(),
                window.role().unwrap_or_default(),
                window.subrole().unwrap_or_default(),
                window.element(),
            );
            commands.entity(entity).set_parent_in_place(app_entity);

            if app.observe_window(&window).is_err() {
                warn!("{}: Error observing window {window_id}.", function_name!());
            }

            window.inner.force_write().psn = Some(app.psn().unwrap());
            let minimized = window.is_minimized();
            let is_root = Window::parent(app.connection().unwrap_or_default(), window.id())
                .is_err()
                || window.is_root();
            {
                let mut inner = window.inner.force_write();
                inner.minimized = minimized;
                inner.is_root = is_root;
            }
            debug!(
                "{}: window {} isroot {} eligible {}",
                function_name!(),
                window.id(),
                window.is_root(),
                window.is_eligible(),
            );

            let Ok(active_display) = displays.single() else {
                return;
            };
            window.update_frame(Some(&active_display.bounds));

            let Ok(panel) = active_display.active_panel(main_cid.0) else {
                return;
            };

            let insert_at = focused_window
                .single()
                .ok()
                .and_then(|(_, window)| panel.index_of(window.id()).ok());
            match insert_at {
                Some(after) => {
                    panel.insert_at(after, window.id());
                }
                None => panel.append(window.id()),
            }

            // self.window_focused(&window);
            let Ok((focused_entity, _)) = focused_window.single() else {
                return;
            };
            commands.entity(focused_entity).remove::<FocusedMarker>();
            commands.entity(entity).insert(FocusedMarker);
        }
    }

    /// Handles the event when a window is moved. Currently, this function does nothing.
    ///
    /// # Arguments
    ///
    /// * `_window_id` - The ID of the window that was moved.
    fn window_moved(&self, _window_id: WinID) {
        //
    }

    /// Handles the event when a window is resized. It updates the window's frame and reshuffles windows.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window that was resized.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the window is resized successfully, otherwise `Err(Error)`.
    fn window_resized<F: Fn(WinID) -> Option<Window>>(
        &self,
        window: &Window,
        active_display: &Display,
        find_window: &F,
    ) -> Result<()> {
        window.update_frame(Some(&active_display.bounds))?;
        self.reshuffle_around(window, active_display, find_window)?;
        Ok(())
    }

    /// Handles the event when a window gains focus. It updates the focused window, PSN, and reshuffles windows.
    /// It also centers the mouse on the focused window if focus-follows-mouse is enabled.
    ///
    /// # Arguments
    ///
    /// * `window` - The `Window` object that gained focus.
    pub fn window_focused(
        applications: Query<&Application>,
        windows: Query<&Window>,
        focused_window: Query<(&Window, Entity, &ChildOf), With<FocusedMarker>>,
        displays: Query<&Display, With<FocusedMarker>>,
        mut window_manager: ResMut<WindowManagerResource>,
        mut commands: Commands,
    ) {
        let (window, entity, child) = match focused_window.single() {
            Ok(ok) => ok,
            Err(QuerySingleError::MultipleEntities(_)) => {
                error!(
                    "{}: Multiple focused windows! {}",
                    function_name!(),
                    focused_window.iter().len()
                );
                return;
            }
            Err(_) => return,
        };
        commands.entity(entity).remove::<FocusedMarker>();
        debug!("{}: {}", function_name!(), window.id());

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

        let window_manager = &mut window_manager.0;
        let focused_id = window_manager.focused_window;
        // TODO: fix
        // let _focused_window = self.find_window(focused_id);

        let my_id = window.id();
        if focused_id.is_none_or(|id| id != my_id) {
            if window_manager.ffm_window_id.is_none_or(|id| id != my_id) {
                // window_manager_center_mouse(wm, window);
                window.center_mouse(window_manager.main_cid);
            }
            window_manager.last_window = focused_id;
        }

        debug!("{}: {} getting focus", function_name!(), my_id);
        debug!("did_receive_focus: {my_id} getting focus");
        window_manager.focused_window = Some(my_id);
        window_manager.focused_psn = app.psn().unwrap();
        window_manager.ffm_window_id = None;

        if window_manager.skip_reshuffle {
            window_manager.skip_reshuffle = false;
        } else {
            let find_window = |window_id| {
                windows
                    .iter()
                    .find(|window| window.id() == window_id)
                    .cloned()
            };
            let Some(active_display) = displays.single().ok() else {
                warn!("{}: Unable to get current window pane.", function_name!());
                return;
            };
            _ = window_manager.reshuffle_around(window, active_display, &find_window);
        }
    }

    /// Reshuffles windows around the given `window` within the active panel to ensure visibility.
    /// Windows to the right and left of the focused window are repositioned.
    ///
    /// # Arguments
    ///
    /// * `window` - A reference to the `Window` around which to reshuffle.
    ///
    /// # Returns
    ///
    /// `Ok(())` if reshuffling is successful, otherwise `Err(Error)`.
    pub fn reshuffle_around<F: Fn(WinID) -> Option<Window>>(
        &self,
        window: &Window,
        active_display: &Display,
        find_window: &F,
    ) -> Result<()> {
        if !window.inner().managed {
            return Ok(());
        }

        let active_panel = active_display.active_panel(self.main_cid)?;
        let display_bounds = &active_display.bounds;
        let frame = window.expose_window(display_bounds);

        let index = active_panel.index_of(window.id())?;
        let panel = active_panel.get(index)?;
        WindowManager::reposition_stack(
            frame.origin.x,
            &panel,
            frame.size.width,
            display_bounds,
            find_window,
        );

        // Shuffling windows to the right of the focus.
        let mut upper_left = frame.origin.x + frame.size.width;
        active_panel.access_right_of(window.id(), |panel| {
            let frame = panel
                .top()
                .and_then(find_window)
                .map(|window| window.inner().frame);
            if let Some(frame) = frame {
                trace!("{}: right: frame: {frame:?}", function_name!());

                // Check for window getting off screen.
                if upper_left > display_bounds.size.width - THRESHOLD {
                    upper_left = display_bounds.size.width - THRESHOLD;
                }

                if (frame.origin.x - upper_left).abs() > 0.1 {
                    WindowManager::reposition_stack(
                        upper_left,
                        panel,
                        frame.size.width,
                        display_bounds,
                        find_window,
                    );
                }
                upper_left += frame.size.width;
            }
            true // continue through all windows
        })?;

        // Shuffling windows to the left of the focus.
        let mut upper_left = frame.origin.x;
        active_panel.access_left_of(window.id(), |panel| {
            let frame = panel
                .top()
                .and_then(find_window)
                .map(|window| window.inner().frame);
            if let Some(frame) = frame {
                trace!("{}: left: frame: {frame:?}", function_name!());

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
                        display_bounds,
                        find_window,
                    );
                }
            }
            true // continue through all windows
        })
    }

    fn reposition_stack<F: Fn(WinID) -> Option<Window>>(
        upper_left: f64,
        panel: &Panel,
        width: f64,
        display_bounds: &CGRect,
        find_window: &F,
    ) {
        let windows = match panel {
            Panel::Single(window_id) => vec![*window_id],
            Panel::Stack(stack) => stack.clone(),
        }
        .iter()
        .filter_map(|window_id| find_window(*window_id))
        .collect::<Vec<_>>();
        let mut y_pos = 0f64;
        let height = display_bounds.size.height / windows.len() as f64;
        for window in windows {
            window.reposition(upper_left, y_pos, display_bounds);
            window.resize(width, height, display_bounds);
            y_pos += height;
        }
    }

    /// Retrieves the currently active display.
    ///
    /// # Returns
    ///
    /// `Ok(&Display)` if an active display is found, otherwise `Err(Error)`.
    pub fn active_display<'a>(displays: &'a Query<&'a Display>) -> Result<&'a Display> {
        displays.single().map_err(|err| {
            Error::new(
                ErrorKind::NotFound,
                format!("{}: active dislay not found: {err}", function_name!()),
            )
        })
    }

    /// Adds an existing process to the window manager. This is used during initial setup for already running applications.
    /// It attempts to create and observe the application and its windows.
    ///
    /// # Arguments
    ///
    /// * `psn` - A reference to the `ProcessSerialNumber` of the existing process.
    /// * `observer` - A `Retained<WorkspaceObserver>` to observe workspace events.
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

    #[allow(clippy::needless_pass_by_value)]
    pub fn add_existing_application(
        cid: Res<MainConnection>,
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
                    WindowManager::add_existing_application_windows(cid.0, &mut app, &spaces, 0)
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
    /// * `psn` - A reference to the `ProcessSerialNumber` of the launched application.
    /// * `observer` - A `Retained<WorkspaceObserver>` to observe workspace events.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the application is launched and processed successfully, otherwise `Err(Error)`.
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

            let app = Application::new(cid.0, &process.0, &events.0).unwrap();

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
                let element = AxuWrapperType::retain(element_ref.as_ptr());
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
    /// # Returns
    ///
    /// `true` if focus follows mouse is enabled, `false` otherwise.
    pub fn focus_follows_mouse(&self) -> bool {
        self.focus_follows_mouse
    }

    /// Checks if the currently focused window is in the active panel.
    /// If not, the window has been moved to a different display or workspace - so the window
    /// manager needs to reorient itself and re-insert the window into correct location.
    #[allow(clippy::needless_pass_by_value)]
    pub fn display_add_remove_trigger(
        trigger: On<DisplayChangeTrigger>,
        // windows: Query<&Window>,
        displays: Query<(&Display, Entity)>,
        main_cid: Res<MainConnection>,
        mut window_manager: ResMut<WindowManagerResource>,
        mut commands: Commands,
    ) {
        let main_cid = main_cid.0;
        let window_manager = &mut window_manager.0;

        match trigger.event().0 {
            Event::DisplayAdded { display_id } => {
                debug!("{}: Display Added: {display_id:?}", function_name!());
                // display_id: CGDirectDisplayID,
                // find_window: &F,
                // commands: &mut Commands,
                let display = Display::present_displays(main_cid)
                    .into_iter()
                    .find(|display| display.id == display_id);
                if let Some(display) = display {
                    // FIXME:
                    // other_displays.push(display);
                    // window_manager.find_orphaned_spaces(other_displays, &find_window);
                    commands.spawn(display);
                }
            }

            Event::DisplayRemoved { display_id } => {
                debug!("{}: Display Removed: {display_id:?}", function_name!());
                if let Some((display, entity)) = displays
                    .iter()
                    .find_map(|(display, entity)| (display.id == display_id).then_some(entity))
                    .and_then(|entity| displays.get(entity).ok())
                {
                    commands.entity(entity).despawn();
                    for (space_id, pane) in &display.spaces {
                        window_manager
                            .orphaned_spaces
                            .insert(*space_id, pane.clone());
                    }
                    // FIXME:
                    // other_displays.retain(|display| display.id != display_id);
                }
                // FIXME:
                // window_manager.find_orphaned_spaces(other_displays, &find_window);
            }

            _ => (),
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn display_change_trigger(
        _: On<DisplayChangeTrigger>,
        focused_window: Query<&Window, With<FocusedMarker>>,
        active_display: Query<&Display, With<FocusedMarker>>,
        displays: Query<(&Display, Entity), Without<FocusedMarker>>,
        main_cid: Res<MainConnection>,
    ) {
        debug!(
            "{}: Display or Workspace changed, reorienting windows.",
            function_name!()
        );
        let main_cid = main_cid.0;
        let Ok(window) = focused_window.single() else {
            return;
        };
        let window_id = window.id();
        let Some(panel) = active_display
            .single()
            .ok()
            .and_then(|display| display.active_panel(main_cid).ok())
        else {
            return;
        };

        if window.managed() && panel.index_of(window_id).is_err() {
            // Current window is not present in the current pane. This is probably due to it being
            // moved to a different desktop. Re-insert it into a correct pane.
            debug!(
                "{}: Window {} moved between displays or workspaces.",
                function_name!(),
                window_id
            );
            // First remove it from all the displays.
            for (display, _) in displays {
                display.remove_window(window_id);
            }

            // .. and then re-insert it into the current one.
            panel.append(window_id);
        }
    }

    fn slide_window<F: Fn(WinID) -> Option<Window>>(
        &self,
        active_display: &Display,
        delta_x: f64,
        find_window: &F,
    ) -> Result<()> {
        trace!("{}: Windows slide {delta_x}.", function_name!());
        let Some(window) = self
            .focused_window
            .and_then(find_window)
            .filter(Window::is_eligible)
        else {
            warn!("{}: No window focused.", function_name!());
            return Ok(());
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
        window.center_mouse(self.main_cid);
        self.reshuffle_around(&window, active_display, find_window)?;
        Ok(())
    }

    /// Finds a window at a given screen point using `SkyLight` API.
    ///
    /// # Arguments
    ///
    /// * `point` - A reference to the `CGPoint` representing the screen coordinate.
    ///
    /// # Returns
    ///
    /// `Ok(Window)` with the found window if successful, otherwise `Err(Error)`.
    fn find_window_at_point<F: Fn(WinID) -> Option<Window>>(
        main_cid: ConnID,
        point: &CGPoint,
        find_window: &F,
    ) -> Result<Window> {
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
        find_window(window_id).ok_or(Error::other(format!(
            "{}: could not find a window at {point:?}",
            function_name!()
        )))
    }

    /// Handles a mouse moved event. If focus-follows-mouse is enabled, it attempts to focus the window under the cursor.
    /// It also handles child windows like sheets and drawers.
    ///
    /// # Arguments
    ///
    /// * `point` - A reference to the `CGPoint` where the mouse moved to.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event is handled successfully or if focus-follows-mouse is disabled, otherwise `Err(Error)`.
    pub fn mouse_moved<F: Fn(WinID) -> Option<Window>>(
        &mut self,
        main_cid: ConnID,
        point: &CGPoint,
        find_window: &F,
    ) -> Result<()> {
        if !self.focus_follows_mouse() {
            return Ok(());
        }
        if self.mission_control_is_active() {
            return Ok(());
        }
        if self.ffm_window_id.is_some() {
            trace!("{}: ffm_window_id > 0", function_name!());
            return Ok(());
        }

        match WindowManager::find_window_at_point(main_cid, point, find_window) {
            Ok(window) => {
                let window_id = window.id();
                if self.focused_window.is_some_and(|id| id == window_id) {
                    trace!("{}: allready focused {}", function_name!(), window_id);
                    return Ok(());
                }
                if !window.is_eligible() {
                    trace!("{}: {} not eligible", function_name!(), window_id);
                    return Ok(());
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
                    let Some(child_window) = find_window(child_wid) else {
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

                    // bool valid = CFEqual(role, kAXSheetRole) || CFEqual(role, kAXDrawerRole);
                    let valid = ["AXSheet", "AXDrawer"]
                        .iter()
                        .any(|axrole| axrole.eq(&role));

                    if valid {
                        window = child_window.clone();
                        break;
                    }
                }

                //  Do not reshuffle windows due to moved mouse focus.
                self.skip_reshuffle = true;

                window.focus_without_raise(self);
                self.ffm_window_id = Some(window_id);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    /// Handles a mouse down event. It finds the window at the click point, reshuffles if necessary,
    /// and stores the clicked window and location.
    ///
    /// # Arguments
    ///
    /// * `point` - A reference to the `CGPoint` where the mouse down occurred.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event is handled successfully, otherwise `Err(Error)`.
    fn mouse_down<F: Fn(WinID) -> Option<Window>>(
        &mut self,
        main_cid: ConnID,
        point: &CGPoint,
        active_display: &Display,
        find_window: &F,
    ) -> Result<()> {
        debug!("{}: {point:?}", function_name!());
        if self.mission_control_is_active() {
            return Ok(());
        }

        let window = WindowManager::find_window_at_point(main_cid, point, find_window)?;
        if !window.fully_visible(&active_display.bounds) {
            self.reshuffle_around(&window, active_display, find_window)?;
        }

        self.mouse_down_window = Some(window);
        self.down_location = *point;

        Ok(())
    }

    /// Handles a mouse up event. Currently, this function does nothing except logging.
    ///
    /// # Arguments
    ///
    /// * `point` - A reference to the `CGPoint` where the mouse up occurred.
    fn mouse_up(&mut self, point: &CGPoint) {
        debug!("{}: {point:?}", function_name!());
    }

    /// Handles a mouse dragged event. Currently, this function does nothing except logging.
    ///
    /// # Arguments
    ///
    /// * `point` - A reference to the `CGPoint` where the mouse was dragged.
    fn mouse_dragged(&self, point: &CGPoint) {
        trace!("{}: {point:?}", function_name!());

        if self.mission_control_is_active() {
            #[warn(clippy::needless_return)]
            return;
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn mouse_trigger(
        trigger: On<MouseTrigger>,
        windows: Query<&Window>,
        displays: Query<&Display, With<FocusedMarker>>,
        mut window_manager: ResMut<WindowManagerResource>,
    ) {
        let find_window = |window_id| {
            windows
                .iter()
                .find(|window| window.id() == window_id)
                .cloned()
        };
        let window_manager = &mut window_manager.0;
        let main_cid = window_manager.main_cid;
        let Ok(active_display) = displays.single() else {
            warn!("{}: Unable to get current display.", function_name!());
            return;
        };
        match &trigger.event().0 {
            Event::MouseDown { point } => {
                window_manager.mouse_down(main_cid, point, active_display, &find_window);
            }
            Event::MouseUp { point } => window_manager.mouse_up(point),
            Event::MouseMoved { point } => {
                window_manager.mouse_moved(main_cid, point, &find_window);
            }
            Event::MouseDragged { point } => window_manager.mouse_dragged(point),
            _ => (),
        }
    }
}

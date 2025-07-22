use core::ptr::NonNull;
use log::{debug, error, info, trace, warn};
use objc2::rc::Retained;
use objc2_core_foundation::{
    CFArrayGetCount, CFDataCreateMutable, CFDataGetMutableBytePtr, CFDataIncreaseLength,
    CFNumberType, CFRetained, CGRect,
};
use objc2_core_graphics::CGDirectDisplayID;
use std::collections::HashMap;
use std::io::{Error, ErrorKind, Result};
use std::ops::Deref;
use std::pin::Pin;
use std::slice::from_raw_parts_mut;
use stdext::function_name;

use crate::app::Application;
use crate::config::Config;
use crate::events::{Event, EventSender};
use crate::platform::{Pid, ProcessSerialNumber, WorkspaceObserver};
use crate::process::Process;
use crate::skylight::{
    _AXUIElementCreateWithRemoteToken, _SLPSGetFrontProcess, ConnID,
    SLSCopyWindowsWithOptionsAndTags, SLSWindowIteratorAdvance, SLSWindowIteratorGetAttributes,
    SLSWindowIteratorGetParentID, SLSWindowIteratorGetTags, SLSWindowIteratorGetWindowID,
    SLSWindowQueryResultCopyWindows, SLSWindowQueryWindows, WinID,
};
use crate::util::{AxuWrapperType, create_array, get_array_values};
use crate::windows::{Display, Panel, Window, WindowPane, ax_window_id, ax_window_pid};

const THRESHOLD: f64 = 10.0;

pub struct WindowManager {
    events: EventSender,
    processes: HashMap<ProcessSerialNumber, Pin<Box<Process>>>,
    main_cid: ConnID,
    last_window: Option<WinID>, // TODO: use this for "goto last window bind"
    pub focused_window: Option<WinID>,
    pub focused_psn: ProcessSerialNumber,
    pub ffm_window_id: Option<WinID>,
    mission_control_is_active: bool,
    pub skip_reshuffle: bool,
    displays: Vec<Display>,
    focus_follows_mouse: bool,
    orphaned_spaces: HashMap<u64, WindowPane>,
}

impl WindowManager {
    /// Creates a new `WindowManager` instance.
    ///
    /// # Arguments
    ///
    /// * `events` - An `EventSender` to send events to the event handler.
    /// * `main_cid` - The main connection ID for the SkyLight API.
    ///
    /// # Returns
    ///
    /// A new `WindowManager` instance.
    pub fn new(events: EventSender, main_cid: ConnID) -> Self {
        WindowManager {
            events,
            processes: HashMap::new(),
            main_cid,
            last_window: None,
            focused_window: None,
            focused_psn: ProcessSerialNumber::default(),
            ffm_window_id: None,
            mission_control_is_active: false,
            skip_reshuffle: false,
            displays: Display::present_displays(main_cid),
            focus_follows_mouse: true,
            orphaned_spaces: HashMap::new(),
        }
    }

    /// Processes an incoming event, dispatching it to the appropriate handler method.
    ///
    /// # Arguments
    ///
    /// * `event` - The `Event` to be processed.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event is processed successfully, otherwise `Err(Error)` if the event is unhandled or an error occurs.
    pub fn process_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::ApplicationTerminated { psn } => self.delete_application(&psn),
            Event::ApplicationFrontSwitched { psn } => self.front_switched(&psn)?,

            Event::WindowCreated { element } => self.window_created(element)?,
            Event::WindowDestroyed { window_id } => self.window_destroyed(window_id),
            Event::WindowMoved { window_id } => self.window_moved(window_id),
            Event::WindowResized { window_id } => self.window_resized(window_id)?,

            Event::Swipe { delta_x } => {
                debug!("Swipe {delta_x}");

                let display_bounds = self.current_display_bounds()?;
                let window = match self
                    .focused_window
                    .and_then(|window_id| self.find_window(window_id))
                    .filter(|window| window.is_eligible())
                {
                    Some(window) => window,
                    None => {
                        warn!("{}: No window focused.", function_name!());
                        return Ok(());
                    }
                };
                let frame = window.frame();
                window.reposition(
                    // Delta is relative to the touchpad size, so to avoid too fast movement we
                    // scale it down by half.
                    frame.origin.x - (display_bounds.size.width / 2.0 * delta_x),
                    frame.origin.y,
                );
                window.center_mouse(self.main_cid);
                self.reshuffle_around(&window)?;
            }

            Event::MissionControlShowAllWindows
            | Event::MissionControlShowFrontWindows
            | Event::MissionControlShowDesktop => {
                self.mission_control_is_active = true;
            }
            Event::MissionControlExit => {
                self.mission_control_is_active = false;
            }

            Event::WindowMinimized { window_id } => {
                self.active_display()?.remove_window(window_id);
            }
            Event::WindowDeminimized { window_id } => {
                self.active_display()?
                    .active_panel(self.main_cid)?
                    .append(window_id);
            }

            Event::ConfigRefresh { config } => {
                self.reload_config(config);
            }

            _ => {
                return Err(Error::new(
                    ErrorKind::NotFound,
                    format!("Unhandled event {event:?}"),
                ));
            }
        }
        Ok(())
    }

    /// Reloads the manager's configuration based on the provided `Config` object.
    ///
    /// # Arguments
    ///
    /// * `config` - The new `Config` object to load.
    fn reload_config(&mut self, config: Config) {
        debug!("{}: Got fresh config: {config:?}", function_name!());
        self.focus_follows_mouse = config.options().focus_follows_mouse;
    }

    fn find_orphaned_spaces(&mut self) {
        let mut relocated_windows = vec![];

        for display in self.displays.iter_mut() {
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
            .flat_map(|(window_id, bounds)| self.find_window(*window_id).zip(Some(bounds)))
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

    pub fn display_remove(&mut self, display_id: CGDirectDisplayID) {
        let removed_index = self
            .displays
            .iter()
            .enumerate()
            .find(|(_, display)| display.id == display_id)
            .map(|(index, _)| index);
        if let Some(index) = removed_index {
            let display = self.displays.remove(index);

            for (space_id, pane) in display.spaces {
                self.orphaned_spaces.insert(space_id, pane);
            }
        }
        self.find_orphaned_spaces();
    }

    pub fn display_add(&mut self, display_id: CGDirectDisplayID) {
        let display = Display::present_displays(self.main_cid)
            .into_iter()
            .find(|display| display.id == display_id);
        if let Some(display) = display {
            self.displays.push(display);
            self.find_orphaned_spaces();
        }
    }

    /// Refreshes the list of active displays and reorganizes windows across them.
    /// It preserves spaces from old displays if they still exist.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the displays are refreshed successfully, otherwise `Err(Error)`.
    pub fn refresh_displays(&mut self) -> Result<()> {
        self.displays = Display::present_displays(self.main_cid);
        if self.displays.is_empty() {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("{}: Can not find any displays?!", function_name!()),
            ));
        }

        for display in self.displays.iter() {
            let display_bounds = display.bounds;
            for (space_id, pane) in display.spaces.iter() {
                self.refresh_windows_space(*space_id, pane);
                pane.all_windows()
                    .iter()
                    .flat_map(|window_id| self.find_window(*window_id))
                    .for_each(|window| {
                        _ = window.update_frame(&display_bounds);
                    });
            }
        }

        Ok(())
    }

    /// Repopulates the current window panel with eligible windows from a specified space.
    ///
    /// # Arguments
    ///
    /// * `space_id` - The ID of the space to refresh windows from.
    /// * `pane` - A reference to the `WindowPane` to which windows will be appended.
    fn refresh_windows_space(&self, space_id: u64, pane: &WindowPane) {
        self.space_window_list_for_connection(vec![space_id], None, false)
            .inspect_err(|err| {
                warn!(
                    "{}: getting windows for space {space_id}: {err}",
                    function_name!()
                )
            })
            .unwrap_or_default()
            .into_iter()
            .flat_map(|window_id| self.find_window(window_id))
            .filter(|window| window.is_eligible())
            .for_each(|window| {
                self.displays
                    .iter()
                    .for_each(|display| display.remove_window(window.inner().id));
                pane.append(window.id())
            });
    }

    /// Finds a process by its `ProcessSerialNumber`.
    ///
    /// # Arguments
    ///
    /// * `psn` - A reference to the `ProcessSerialNumber` of the process to find.
    ///
    /// # Returns
    ///
    /// `Some(&Process)` if the process is found, otherwise `None`.
    fn find_process(&self, psn: &ProcessSerialNumber) -> Option<&Process> {
        self.processes.get(psn).map(|process| process.deref())
    }

    /// Finds an `Application` by its process ID (Pid).
    ///
    /// # Arguments
    ///
    /// * `pid` - The process ID of the application to find.
    ///
    /// # Returns
    ///
    /// `Some(Application)` if the application is found, otherwise `None`.
    fn find_application(&self, pid: Pid) -> Option<Application> {
        self.processes
            .values()
            .find(|process| process.pid == pid)
            .and_then(|process| process.get_app())
    }

    /// Finds a `Window` by its window ID across all managed processes.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to find.
    ///
    /// # Returns
    ///
    /// `Some(Window)` if the window is found, otherwise `None`.
    pub fn find_window(&self, window_id: WinID) -> Option<Window> {
        self.processes
            .values()
            .flat_map(|process| process.get_app().and_then(|app| app.find_window(window_id)))
            .next()
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
    /// This function uses SkyLight API calls.
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
    fn space_window_list_for_connection(
        &self,
        spaces: Vec<u64>,
        cid: Option<ConnID>,
        also_minimized: bool,
    ) -> Result<Vec<WinID>> {
        unsafe {
            let space_list_ref = create_array(spaces, CFNumberType::SInt64Type)?;

            let mut set_tags = 0i64;
            let mut clear_tags = 0i64;
            let options = if also_minimized { 0x7 } else { 0x2 };
            let ptr = NonNull::new(SLSCopyWindowsWithOptionsAndTags(
                self.main_cid,
                cid.unwrap_or(0),
                space_list_ref.deref(),
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

            let count = CFArrayGetCount(window_list_ref.deref());
            if count == 0 {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!("{}: zero windows returned", function_name!()),
                ));
            }

            let query = CFRetained::from_raw(SLSWindowQueryWindows(
                self.main_cid,
                window_list_ref.deref(),
                count,
            ));
            let iterator =
                CFRetained::from_raw(SLSWindowQueryResultCopyWindows(query.deref().into()));

            let mut window_list = Vec::with_capacity(count.try_into().unwrap());
            while SLSWindowIteratorAdvance(iterator.deref()) {
                let tags = SLSWindowIteratorGetTags(iterator.deref());
                let attributes = SLSWindowIteratorGetAttributes(iterator.deref());
                let parent_wid: WinID = SLSWindowIteratorGetParentID(iterator.deref());
                let wid: WinID = SLSWindowIteratorGetWindowID(iterator.deref());

                trace!(
                    "{}: id: {wid} parent: {parent_wid} tags: 0x{tags:x} attributes: 0x{attributes:x}",
                    function_name!()
                );
                match self.find_window(wid) {
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
            && ((0 != (attributes & 0x2) || 0 != (tags & 0x400000000000000))
                && (0 != (tags & 0x1) || (0 != (tags & 0x2) && 0 != (tags & 0x80000000))))
            || ((attributes == 0x0 || attributes == 0x1)
                && (0 != (tags & 0x1000000000000000) || 0 != (tags & 0x300000000000000))
                && (0 != (tags & 0x1) || (0 != (tags & 0x2) && 0 != (tags & 0x80000000))))
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
    fn existing_application_window_list(&self, app: &Application) -> Result<Vec<WinID>> {
        let spaces: Vec<u64> = self
            .displays
            .iter()
            .flat_map(|display| display.spaces.keys().cloned().collect::<Vec<_>>())
            .collect();
        debug!("{}: spaces {spaces:?}", function_name!());
        // return space_list ? space_window_list_for_connection(
        // space_list, space_count, application ? application->connection : 0, window_count, true) : NULL;
        if spaces.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("{}: no spaces returned", function_name!()),
            ));
        }
        self.space_window_list_for_connection(spaces, app.connection().ok(), true)
    }

    /// Attempts to find and add unresolved windows for a given application by brute-forcing `element_id` values.
    /// This is a workaround for macOS API limitations that do not return `AXUIElementRef` for windows on inactive spaces.
    ///
    /// # Arguments
    ///
    /// * `app` - A reference to the `Application` whose windows are to be brute-forced.
    /// * `window_list` - A mutable vector of `WinID`s representing the expected global window list; found windows are removed from this list.
    fn bruteforce_windows(&mut self, app: &Application, window_list: &mut Vec<WinID>) {
        debug!(
            "{}: App {} has unresolved window on other desktops, bruteforcing them.",
            function_name!(),
            app.name().unwrap()
        );

        //
        // NOTE: MacOS API does not return AXUIElementRef of windows on inactive spaces. However,
        // we can just brute-force the element_id and create the AXUIElementRef ourselves.
        //  https://github.com/decodism
        //  https://github.com/lwouis/alt-tab-macos/issues/1324#issuecomment-2631035482
        //

        unsafe {
            const BUFSIZE: isize = 0x14;
            let data_ref = match CFDataCreateMutable(None, BUFSIZE) {
                Some(data) => data,
                None => {
                    error!("{}: error creating mutable data", function_name!());
                    return;
                }
            };
            CFDataIncreaseLength(data_ref.deref().into(), BUFSIZE);

            const MAGIC: u32 = 0x636f636f;
            let data = from_raw_parts_mut(
                CFDataGetMutableBytePtr(data_ref.deref().into()),
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

                let element_ref = match AxuWrapperType::retain(_AXUIElementCreateWithRemoteToken(
                    data_ref.as_ref(),
                )) {
                    Ok(element_ref) => element_ref,
                    _ => continue,
                };
                let window_id = match ax_window_id(element_ref.as_ptr()) {
                    Ok(window_id) => window_id,
                    _ => continue,
                };

                if let Some(index) = window_list.iter().position(|&id| id == window_id) {
                    window_list.remove(index);
                    debug!("{}: Found window {window_id:?}", function_name!());
                    _ = self
                        .create_and_add_window(app, element_ref, window_id, false)
                        .inspect_err(|err| warn!("{}: {err}", function_name!()));
                }
            }
        }
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
        &mut self,
        app: &Application,
        refresh_index: i32,
    ) -> Result<bool> {
        let mut result = false;
        let name = app.name()?;

        let global_window_list = self.existing_application_window_list(app)?;
        if global_window_list.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("{}: No windows found for app {}", function_name!(), name,),
            ));
        }
        info!(
            "{}: App {} has global windows: {global_window_list:?}",
            function_name!(),
            name
        );

        let window_list = app.window_list();
        let window_count = window_list
            .as_ref()
            .map(|window_list| unsafe { CFArrayGetCount(window_list) })
            .unwrap_or(0);

        let mut empty_count = 0;
        if let Ok(window_list) = window_list {
            for window_ref in get_array_values(window_list.deref()) {
                let window_id = match ax_window_id(window_ref.as_ptr()) {
                    Ok(window_id) => window_id,
                    Err(_) => {
                        empty_count += 1;
                        continue;
                    }
                };

                //
                // FIXME: The AX API appears to always include a single element for Finder that
                // returns an empty window id. This is likely the desktop window. Other similar
                // cases should be handled the same way; simply ignore the window when we attempt
                // to do an equality check to see if we have correctly discovered the number of
                // windows to track.
                //

                if self.find_window(window_id).is_none() {
                    let window_ref = AxuWrapperType::retain(window_ref.as_ptr())?;
                    info!("{}: Add window: {} {window_id}", function_name!(), name);
                    _ = self
                        .create_and_add_window(app, window_ref, window_id, false)
                        .inspect_err(|err| debug!("{}: {err}", function_name!()));
                }
            }
        }

        if global_window_list.len() as isize == (window_count - empty_count) {
            if refresh_index != -1 {
                info!(
                    "{}: All windows for {} are now resolved",
                    function_name!(),
                    name
                );
                result = true;
            }
        } else {
            let mut app_window_list: Vec<WinID> = global_window_list
                .iter()
                .flat_map(|window_id| self.find_window(*window_id).is_none().then_some(window_id))
                .cloned()
                .collect();

            if !app_window_list.is_empty() {
                info!(
                    "{}: {} has windows that are not yet resolved",
                    function_name!(),
                    name
                );
                self.bruteforce_windows(app, &mut app_window_list);
            }
        }

        Ok(result)
    }

    /// Adds new application windows by querying the application's window list.
    /// It filters out existing windows and creates new `Window` objects for unseen ones.
    ///
    /// # Arguments
    ///
    /// * `app` - A reference to the `Application` whose windows are to be added.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<Window>)` containing the newly added `Window` objects if successful, otherwise `Err(Error)`.
    fn add_application_windows(&mut self, app: &Application) -> Result<Vec<Window>> {
        // TODO: maybe refactor this with add_existing_application_windows()
        let array = app.window_list()?;
        let create_window = |element_ref: NonNull<_>| {
            let element = AxuWrapperType::retain(element_ref.as_ptr());
            element.map(|element| {
                let window_id = ax_window_id(element.as_ptr())
                    .inspect_err(|err| warn!("{}: error adding window: {err}", function_name!()))
                    .ok()?;
                self.find_window(window_id).map_or_else(
                    // Window does not exist, create it.
                    || {
                        self.create_and_add_window(app, element, window_id, true)
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
        let windows: Vec<Window> =
            get_array_values::<accessibility_sys::__AXUIElement>(array.deref())
                .flat_map(create_window)
                .flatten()
                .collect();
        Ok(windows)
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
    fn create_and_add_window(
        &mut self,
        app: &Application,
        window_ref: CFRetained<AxuWrapperType>,
        window_id: WinID,
        _one_shot_rules: bool, // TODO: fix
    ) -> Result<Window> {
        let name = app.name()?;
        let window = Window::new(window_id, app, window_ref)?;
        if window.is_unknown() {
            return Err(Error::other(format!(
                "{}: Ignoring AXUnknown window, app: {} id: {}",
                function_name!(),
                name,
                window.id()
            )));
        }

        if !window.is_real() {
            return Err(Error::other(format!(
                "{}: Ignoring non-real window, app: {} id: {}",
                function_name!(),
                name,
                window.id()
            )));
        }

        info!(
            "{}: created {} app: {} title: {} role: {} subrole: {}",
            function_name!(),
            window.id(),
            name,
            window.title().unwrap_or_default(),
            window.role().unwrap_or_default(),
            window.subrole().unwrap_or_default(),
        );

        app.observe_window(window.element(), &window)?;

        app.add_window(&window);
        window.update_frame(&self.current_display_bounds()?)?;
        Ok(window)
    }

    /// Retrieves the currently focused application.
    ///
    /// # Returns
    ///
    /// `Ok(Application)` if a focused application is found, otherwise `Err(Error)`.
    fn focused_application(&self) -> Result<Application> {
        let mut psn = ProcessSerialNumber::default();
        unsafe {
            _SLPSGetFrontProcess(&mut psn);
        }
        self.find_process(&psn)
            .and_then(|process| process.get_app())
            .ok_or(Error::new(
                ErrorKind::NotFound,
                format!(
                    "{}: can not find currently focused application.",
                    function_name!()
                ),
            ))
    }

    /// Retrieves the currently focused window.
    ///
    /// # Returns
    ///
    /// `Ok(Window)` if a focused window is found, otherwise `Err(Error)`.
    fn focused_window(&self) -> Result<Window> {
        let app = self.focused_application()?;
        let window_id = app.focused_window_id()?;
        self.find_window(window_id).ok_or(Error::new(
            ErrorKind::NotFound,
            format!(
                "{}: can not find currently focused window {window_id}.",
                function_name!()
            ),
        ))
    }

    /// Sets the currently focused window and reshuffles windows around it.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the focused window is set successfully, otherwise `Err(Error)`.
    pub fn set_focused_window(&mut self) -> Result<()> {
        if let Ok(window) = self.focused_window() {
            self.last_window = Some(window.id());
            self.focused_window = Some(window.id());
            self.focused_psn = window.app().psn()?;
            self.reshuffle_around(&window)?;
        }
        Ok(())
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
    fn front_switched(&mut self, psn: &ProcessSerialNumber) -> Result<()> {
        let process = self.find_process(psn).ok_or(std::io::Error::new(
            ErrorKind::NotFound,
            format!("{}: Psn {:?} not found.", function_name!(), psn),
        ))?;
        let app = process.get_app().ok_or(std::io::Error::new(
            ErrorKind::NotFound,
            format!(
                "{}: No application for process {}.",
                function_name!(),
                process.name
            ),
        ))?;
        debug!("{}: {}", function_name!(), app.name()?);

        match app.focused_window_id() {
            Err(_) => {
                let focused_window = self
                    .focused_window
                    .and_then(|window_id| self.find_window(window_id));
                if focused_window.is_none() {
                    warn!("{}: window_manager_set_window_opacity", function_name!());
                }

                self.last_window = self.focused_window;
                self.focused_window = None;
                self.focused_psn = app.psn()?;
                self.ffm_window_id = None;
                warn!("{}: reset focused window", function_name!());
            }
            Ok(focused_id) => {
                if let Some(window) = self.find_window(focused_id) {
                    self.window_focused(window);
                } else {
                    warn!(
                        "{}: window_manager_add_lost_focused_event",
                        function_name!()
                    );
                }
            }
        }
        Ok(())
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
    fn window_created(&mut self, ax_element: CFRetained<AxuWrapperType>) -> Result<()> {
        let window_id = ax_window_id(ax_element.as_ptr())?;
        if self.find_window(window_id).is_some() {
            return Err(Error::new(
                ErrorKind::AlreadyExists,
                format!("{}: window {window_id} already created.", function_name!()),
            ));
        }

        let pid = ax_window_pid(&ax_element)?;
        let app = self.find_application(pid).ok_or(Error::new(
            ErrorKind::NotFound,
            format!(
                "{}: unable to find application with {pid}.",
                function_name!()
            ),
        ))?;

        let window = self.create_and_add_window(&app, ax_element, window_id, true)?;
        info!(
            "{}: created {} app: {} title: {} role: {} subrole: {} element: {:x?}",
            function_name!(),
            window.id(),
            app.name()?,
            window.title().unwrap_or_default(),
            window.role().unwrap_or_default(),
            window.subrole().unwrap_or_default(),
            window.element(),
        );

        let panel = self.active_display()?.active_panel(self.main_cid)?;
        let insert_at = self
            .focused_window
            .and_then(|window_id| panel.index_of(window_id).ok());
        match insert_at {
            Some(after) => {
                panel.insert_at(after, window.id())?;
            }
            None => panel.append(window.id()),
        };

        self.window_focused(window);
        Ok(())
    }

    /// Handles the event when a window is destroyed. It removes the window from all displays and the owning application.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window that was destroyed.
    fn window_destroyed(&mut self, window_id: WinID) {
        self.displays.iter().for_each(|display| {
            display.remove_window(window_id);
        });

        let app = self.find_window(window_id).map(|window| window.app());
        if let Some(window) = app.and_then(|app| {
            app.remove_window(window_id)
                .inspect(|window| app.unobserve_window(window.element()))
        }) {
            // Make sure window lives past the lock above, because its Drop tries to lock the
            // application.
            info!("{}: {window_id}", function_name!());
            drop(window)
        }

        let previous = self
            .focused_window
            .and_then(|previous| self.find_window(previous));
        if let Some(window) = previous {
            _ = self.reshuffle_around(&window);
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
    fn window_resized(&self, window_id: WinID) -> Result<()> {
        if let Some(window) = self.find_window(window_id) {
            window.update_frame(&self.current_display_bounds()?)?;
            self.reshuffle_around(&window)?;
        }
        Ok(())
    }

    /// Handles the event when a window gains focus. It updates the focused window, PSN, and reshuffles windows.
    /// It also centers the mouse on the focused window if focus-follows-mouse is enabled.
    ///
    /// # Arguments
    ///
    /// * `window` - The `Window` object that gained focus.
    pub fn window_focused(&mut self, window: Window) {
        let focused_id = self.focused_window;
        // TODO: fix
        // let _focused_window = self.find_window(focused_id);

        let my_id = window.id();
        if focused_id.is_none_or(|id| id != my_id) {
            if self.ffm_window_id.is_none_or(|id| id != my_id) {
                // window_manager_center_mouse(wm, window);
                window.center_mouse(self.main_cid);
            }
            self.last_window = focused_id;
        }

        debug!("{}: {} getting focus", function_name!(), my_id);
        debug!("did_receive_focus: {} getting focus", my_id);
        self.focused_window = Some(my_id);
        self.focused_psn = window.app().psn().unwrap();
        self.ffm_window_id = None;

        if self.skip_reshuffle {
            self.skip_reshuffle = false;
        } else {
            _ = self.reshuffle_around(&window);
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
    pub fn reshuffle_around(&self, window: &Window) -> Result<()> {
        if !window.inner().managed {
            return Ok(());
        }

        let active_display = self.active_display()?;
        let active_panel = active_display.active_panel(self.main_cid)?;
        let display_bounds = self.current_display_bounds()?;
        let frame = window.expose_window(&display_bounds);

        let index = active_panel.index_of(window.id())?;
        let panel = active_panel.get(index)?;
        self.reposition_stack(frame.origin.x, &panel, frame.size.width, &display_bounds);

        // Shuffling windows to the right of the focus.
        let mut upper_left = frame.origin.x + frame.size.width;
        active_panel.access_right_of(window.id(), |panel| {
            let window_id = panel.top().unwrap();
            let window = match self.find_window(window_id) {
                Some(window) => window,
                None => return true,
            };
            let frame = window.inner().frame;
            trace!("{}: right: frame: {frame:?}", function_name!());

            // Check for window getting off screen.
            if upper_left > display_bounds.size.width - THRESHOLD {
                upper_left = display_bounds.size.width - THRESHOLD;
            }

            if frame.origin.x != upper_left {
                self.reposition_stack(upper_left, panel, frame.size.width, &display_bounds);
            }
            upper_left += frame.size.width;
            true // continue through all windows
        })?;

        // Shuffling windows to the left of the focus.
        let mut upper_left = frame.origin.x;
        active_panel.access_left_of(window.id(), |panel| {
            let window_id = panel.top().unwrap();
            let window = match self.find_window(window_id) {
                Some(window) => window,
                None => return true,
            };
            let frame = window.inner().frame;
            trace!("{}: left: frame: {frame:?}", function_name!());

            // Check for window getting off screen.
            if upper_left < THRESHOLD {
                upper_left = THRESHOLD;
            }
            upper_left -= frame.size.width;

            if frame.origin.x != upper_left {
                self.reposition_stack(upper_left, panel, frame.size.width, &display_bounds);
            }
            true // continue through all windows
        })
    }

    fn reposition_stack(
        &self,
        upper_left: f64,
        panel: &Panel,
        width: f64,
        display_bounds: &CGRect,
    ) {
        let windows = match panel {
            Panel::Single(window_id) => vec![*window_id],
            Panel::Stack(stack) => stack.to_vec(),
        }
        .iter()
        .flat_map(|window_id| self.find_window(*window_id))
        .collect::<Vec<_>>();
        let mut y_pos = 0f64;
        let height = display_bounds.size.height / windows.len() as f64;
        for window in windows {
            window.reposition(upper_left, y_pos);
            window.resize(width, height, display_bounds);
            y_pos += height;
        }
    }

    /// Retrieves the currently active display.
    ///
    /// # Returns
    ///
    /// `Ok(&Display)` if an active display is found, otherwise `Err(Error)`.
    pub fn active_display(&self) -> Result<&Display> {
        let id = Display::active_display_id(self.main_cid)?;
        self.displays
            .iter()
            .find(|display| display.id == id)
            .ok_or(Error::new(
                ErrorKind::NotFound,
                format!("{}: can not find active display.", function_name!()),
            ))
    }

    /// Deletes an application and all its associated windows from the manager.
    ///
    /// # Arguments
    ///
    /// * `psn` - A reference to the `ProcessSerialNumber` of the application to delete.
    fn delete_application(&mut self, psn: &ProcessSerialNumber) {
        debug!("{}: {psn:?}", function_name!(),);
        if let Some(app) = self
            .processes
            .remove(psn)
            .and_then(|process| process.get_app())
        {
            app.foreach_window(|window| {
                self.displays.iter().for_each(|display| {
                    display.remove_window(window.id());
                })
            });
        }
    }

    /// Retrieves the bounds (CGRect) of the current active display.
    ///
    /// # Returns
    ///
    /// `Ok(CGRect)` if the display bounds are successfully retrieved, otherwise `Err(Error)`.
    pub fn current_display_bounds(&self) -> Result<CGRect> {
        self.active_display().map(|display| display.bounds)
    }

    /// Adds an existing process to the window manager. This is used during initial setup for already running applications.
    /// It attempts to create and observe the application and its windows.
    ///
    /// # Arguments
    ///
    /// * `psn` - A reference to the `ProcessSerialNumber` of the existing process.
    /// * `observer` - A `Retained<WorkspaceObserver>` to observe workspace events.
    pub fn add_existing_process(
        &mut self,
        psn: &ProcessSerialNumber,
        observer: Retained<WorkspaceObserver>,
    ) {
        let cid = self.main_cid;
        let events = self.events.clone();
        let mut process = Process::new(psn, observer);

        if process.is_observable() {
            self.processes.insert(psn.clone(), process);
            let process = self
                .processes
                .get_mut(psn)
                .expect("Unable to find created process");
            let app = process.create_application(cid, events).unwrap();
            debug!(
                "{}: Application {} is observable",
                function_name!(),
                app.name().unwrap()
            );

            if app.observe().is_ok_and(|result| result) {
                _ = self
                    .add_existing_application_windows(&app, 0)
                    .inspect_err(|err| warn!("{}: {err}", function_name!()));
            }
        } else {
            debug!(
                "{}: Existing application {} is not observable, ignoring it.",
                function_name!(),
                process.name,
            );
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
    pub fn application_launched(
        &mut self,
        psn: &ProcessSerialNumber,
        observer: Retained<WorkspaceObserver>,
    ) -> Result<()> {
        if self.find_process(psn).is_none() {
            let process = Process::new(psn, observer);
            self.processes.insert(psn.clone(), process);
        }

        {
            let process = self.processes.get_mut(psn).ok_or(Error::new(
                ErrorKind::OutOfMemory,
                format!("{}: unable to find created process.", function_name!()),
            ))?;
            if process.terminated {
                return Err(Error::new(
                    ErrorKind::UnexpectedEof,
                    format!(
                        "{}: {} ({}) terminated during launch",
                        function_name!(),
                        process.name,
                        process.pid
                    ),
                ));
            }

            if !process.ready() {
                return Ok(());
            }
        }

        //
        // NOTE: If we somehow receive a duplicate launched event due to the
        // subscription-timing-mess above, simply ignore the event..
        //

        if self
            .find_process(psn)
            .is_some_and(|process| process.get_app().is_some())
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("{}: App {psn:?} already exists.", function_name!()),
            ));
        }

        let process = self.processes.get_mut(psn).unwrap();
        let app = process.create_application(self.main_cid, self.events.clone())?;
        debug!(
            "{}: Added {} to list of apps.",
            function_name!(),
            app.name()?
        );

        if !app.observe()? {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                format!(
                    "{}: failed to register some observers {}",
                    function_name!(),
                    app.name()?
                ),
            ));
        }

        let windows = self.add_application_windows(&app)?;
        debug!(
            "{}: Added windows {} for {}.",
            function_name!(),
            windows
                .iter()
                .map(|window| format!("{}", window.id()))
                .collect::<Vec<_>>()
                .join(", "),
            app.name()?
        );

        let active_panel = self.active_display()?.active_panel(self.main_cid)?;
        let insert_at = self
            .focused_window
            .and_then(|window_id| active_panel.index_of(window_id).ok());
        match insert_at {
            Some(mut after) => {
                for window in &windows {
                    after = active_panel.insert_at(after, window.id())?;
                }
            }
            None => windows.iter().for_each(|window| {
                active_panel.append(window.id());
            }),
        };

        if let Some(window) = windows.first() {
            self.reshuffle_around(window)?;
        }

        Ok(())
    }

    /// Checks if the "focus follows mouse" feature is enabled.
    ///
    /// # Returns
    ///
    /// `true` if focus follows mouse is enabled, `false` otherwise.
    pub fn focus_follows_mouse(&self) -> bool {
        self.focus_follows_mouse
    }
}

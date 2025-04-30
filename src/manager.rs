use core::ptr::NonNull;
use log::{debug, error, info, trace, warn};
use objc2::rc::Retained;
use objc2_core_foundation::{
    CFArrayGetCount, CFDataCreateMutable, CFDataGetMutableBytePtr, CFDataIncreaseLength,
    CFNumberType, CFRetained, CGRect,
};
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
use crate::windows::{Display, Window, WindowPane, ax_window_id, ax_window_pid};

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
}

impl WindowManager {
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
        }
    }

    pub fn process_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::ApplicationTerminated { psn } => self.delete_application(&psn),
            Event::ApplicationFrontSwitched { psn } => self.front_switched(&psn)?,

            Event::WindowCreated { element } => self.window_created(element)?,
            Event::WindowDestroyed { window_id } => self.window_destroyed(window_id),
            Event::WindowMoved { window_id } => self.window_moved(window_id),
            Event::WindowResized { window_id } => self.window_resized(window_id)?,

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

    fn reload_config(&mut self, config: Config) {
        debug!("{}: Got fresh config: {config:?}", function_name!());
        self.focus_follows_mouse = config.options().focus_follows_mouse;
    }

    pub fn refresh_displays(&mut self) -> Result<()> {
        let old_displays = std::mem::take(&mut self.displays);
        self.displays = Display::present_displays(self.main_cid);
        if self.displays.is_empty() {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("{}: Can not find any displays?!", function_name!()),
            ));
        }

        for old_display in old_displays {
            if self
                .displays
                .iter()
                .all(|display| old_display.id != display.id)
            {
                let id = Display::active_display_id(self.main_cid)?;
                if let Some(current) = self.displays.iter_mut().find(|display| display.id == id) {
                    current.spaces = old_display.spaces;
                } else {
                    let id = Display::active_display_id(self.main_cid)?;
                    if let Some(current) = self.displays.iter_mut().find(|display| display.id == id)
                    {
                        current.spaces = old_display.spaces;
                    }
                }
            }
        }

        let display_bounds = self.current_display_bounds()?;
        for display in self.displays.iter() {
            for (space_id, pane) in display.spaces.iter() {
                self.refresh_windows_space(*space_id, pane);

                // Adjust window sizes to the current display.
                pane.access_right_of(pane.first()?, |window_id| {
                    if let Some(window) = self.find_window(window_id) {
                        _ = window.update_frame(&display_bounds);
                    }
                    true // continue through all windows.
                })?;
                if let Some(window) = self.find_window(pane.first()?) {
                    _ = window.update_frame(&display_bounds);
                }
            }
        }

        Ok(())
    }

    // Repopulates current window panel with window from the selected space.
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

    fn find_process(&self, psn: &ProcessSerialNumber) -> Option<&Process> {
        self.processes.get(psn).map(|process| process.deref())
    }

    fn find_application(&self, pid: Pid) -> Option<Application> {
        self.processes
            .values()
            .find(|process| process.pid == pid)
            .and_then(|process| process.get_app())
    }

    pub fn find_window(&self, window_id: WinID) -> Option<Window> {
        self.processes
            .values()
            .flat_map(|process| process.get_app().and_then(|app| app.find_window(window_id)))
            .next()
    }

    pub fn mission_control_is_active(&self) -> bool {
        self.mission_control_is_active
    }

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

    fn found_valid_window(parent_wid: WinID, attributes: i64, tags: i64) -> bool {
        parent_wid == 0
            && ((0 != (attributes & 0x2) || 0 != (tags & 0x400000000000000))
                && (0 != (tags & 0x1) || (0 != (tags & 0x2) && 0 != (tags & 0x80000000))))
            || ((attributes == 0x0 || attributes == 0x1)
                && (0 != (tags & 0x1000000000000000) || 0 != (tags & 0x300000000000000))
                && (0 != (tags & 0x1) || (0 != (tags & 0x2) && 0 != (tags & 0x80000000))))
    }

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

    pub fn set_focused_window(&mut self) -> Result<()> {
        if let Ok(window) = self.focused_window() {
            self.last_window = Some(window.id());
            self.focused_window = Some(window.id());
            self.focused_psn = window.app().psn()?;
            self.reshuffle_around(&window)?;
        }
        Ok(())
    }

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

    fn window_moved(&self, _window_id: WinID) {
        //
    }

    fn window_resized(&self, window_id: WinID) -> Result<()> {
        if let Some(window) = self.find_window(window_id) {
            window.update_frame(&self.current_display_bounds()?)?;
            self.reshuffle_around(&window)?;
        }
        Ok(())
    }

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

    pub fn reshuffle_around(&self, window: &Window) -> Result<()> {
        if !window.inner().managed {
            return Ok(());
        }

        let active_display = self.active_display()?;
        let active_panel = active_display.active_panel(self.main_cid)?;
        let display_bounds = self.current_display_bounds()?;
        let frame = window.expose_window(&display_bounds);

        // Shuffling windows to the right of the focus.
        let mut upper_left = frame.origin.x + frame.size.width;
        active_panel.access_right_of(window.id(), |window_id| {
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
                window.reposition(upper_left, frame.origin.y);
                trace!(
                    "{}: right side moved to upper_left {upper_left}",
                    function_name!()
                );
            }
            upper_left += frame.size.width;
            true // continue through all windows
        })?;

        // Shuffling windows to the left of the focus.
        let mut upper_left = frame.origin.x;
        trace!("{}: focus upper_left {upper_left}", function_name!());
        active_panel.access_left_of(window.id(), |window_id| {
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
                window.reposition(upper_left, frame.origin.y);
                trace!(
                    "{}: left side moved to upper_left {upper_left}",
                    function_name!()
                );
            }
            true // continue through all windows
        })
    }

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

    pub fn current_display_bounds(&self) -> Result<CGRect> {
        self.active_display().map(|display| display.bounds)
    }

    // Add the process without waiting for it to be fully launched, because it is already running -
    // we are calling this at the start fo the window manager.
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

    pub fn focus_follows_mouse(&self) -> bool {
        self.focus_follows_mouse
    }
}

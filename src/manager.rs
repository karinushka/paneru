use accessibility_sys::{AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt};
use bevy::ecs::resource::Resource;
use bevy::math::{IRect, IVec2};
use core::ptr::NonNull;
use derive_more::{DerefMut, with_trait::Deref};
use notify::{RecursiveMode, Watcher};
use objc2_core_foundation::{
    CFArray, CFDictionary, CFMutableData, CFNumber, CFNumberType, CFRetained, CFString, CGPoint,
    CGRect, CGSize, kCFBooleanTrue,
};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGError, CGGetActiveDisplayList, CGRectContainsPoint,
    CGWarpMouseCursorPosition,
};
use std::path::Path;
use std::ptr::null_mut;
use std::slice::from_raw_parts_mut;
use std::time::Duration;
use stdext::function_name;
use tracing::{Level, debug, error, instrument, trace, warn};

use crate::errors::{Error, Result};
use crate::events::{Event, EventSender};
use crate::platform::{ConnID, Pid, ProcessSerialNumber, WinID, WorkspaceId};
use crate::util::{AXUIWrapper, MacResult, create_array, symlink_target};
use app::ApplicationOS;
pub use app::{Application, ApplicationApi};
pub use display::Display;
pub use layout::{Column, LayoutStrip};
pub use process::{Process, ProcessApi};
pub use skylight::AXUIElementCopyAttributeValue;
use skylight::{
    _AXUIElementCreateWithRemoteToken, SLSCopyActiveMenuBarDisplayIdentifier,
    SLSCopyAssociatedWindows, SLSCopyBestManagedDisplayForRect, SLSCopyManagedDisplayForWindow,
    SLSCopyManagedDisplaySpaces, SLSCopyWindowsWithOptionsAndTags, SLSFindWindowAndOwner,
    SLSGetConnectionIDForPSN, SLSGetCurrentCursorLocation, SLSGetDisplayMenubarHeight,
    SLSGetSpaceManagementMode, SLSGetWindowBounds, SLSMainConnectionID,
    SLSManagedDisplayGetCurrentSpace, SLSWindowIteratorAdvance, SLSWindowIteratorGetAttributes,
    SLSWindowIteratorGetParentID, SLSWindowIteratorGetTags, SLSWindowIteratorGetWindowID,
    SLSWindowQueryResultCopyWindows, SLSWindowQueryWindows,
};
pub use windows::{Window, WindowApi, WindowOS, WindowPadding, ax_window_id};

mod app;
mod display;
mod layout;
mod process;
mod skylight;
mod windows;

pub type Origin = IVec2;
pub type Size = IVec2;

#[allow(clippy::cast_possible_truncation)]
pub fn origin_from(point: CGPoint) -> Origin {
    Origin::new(point.x as i32, point.y as i32)
}

pub fn origin_to(point: Origin) -> CGPoint {
    CGPoint::new(point.x.into(), point.y.into())
}

#[allow(clippy::cast_possible_truncation)]
pub fn size_from(size: CGSize) -> Size {
    Size::new(size.width as i32, size.height as i32)
}

pub fn irect_from(rect: CGRect) -> IRect {
    let mid = rect.mid();
    IRect::from_center_size(origin_from(mid), size_from(rect.size))
}

/// Defines the interface for a window manager, abstracting OS-specific operations.
pub trait WindowManagerApi: Send + Sync {
    /// Creates a new `Application` instance from a given `ProcessApi`.
    ///
    /// # Arguments
    ///
    /// * `process` - A reference to the `ProcessApi` trait object representing the application's process.
    ///
    /// # Returns
    ///
    /// `Ok(Application)` if the application is successfully created, otherwise `Err(Error)`.
    fn new_application(&self, process: &dyn ProcessApi) -> Result<Application>;
    /// Retrieves a list of window IDs associated with a parent window.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The `WinID` of the parent window.
    ///
    /// # Returns
    ///
    /// A `Vec<WinID>` containing the IDs of associated child windows.
    fn get_associated_windows(&self, window_id: WinID) -> Vec<WinID>;
    /// Retrieves a list of all currently present displays.
    ///
    /// # Returns
    ///
    /// A `Vec<Display>` containing `Display` objects for all present displays.
    fn present_displays(&self) -> Vec<(Display, Vec<WorkspaceId>)>;
    /// Retrieves the `CGDirectDisplayID` of the active menu bar display.
    ///
    /// # Returns
    ///
    /// `Ok(u32)` with the display ID if successful, otherwise `Err(Error)`.
    fn active_display_id(&self) -> Result<u32>;
    /// Retrieves the ID of the current active space on a given display.
    ///
    /// # Arguments
    ///
    /// * `display_id` - The `CGDirectDisplayID` of the display.
    ///
    /// # Returns
    ///
    /// `Ok(u64)` with the space ID if successful, otherwise `Err(Error)`.
    fn active_display_space(&self, display_id: CGDirectDisplayID) -> Result<WorkspaceId>;
    /// Centers the mouse cursor on a given window within its display bounds if it's not already within the window.
    ///
    /// # Arguments
    ///
    /// * `window` - A reference to the `Window` to center the mouse on.
    /// * `display_bounds` - The `CGRect` representing the bounds of the display the window is on.
    fn center_mouse(&self, window: Option<&Window>, display_bounds: &IRect);
    /// Adds existing windows for a given application, potentially resolving unresolved windows.
    ///
    /// # Arguments
    ///
    /// * `app` - A mutable reference to the `Application` whose windows are to be added.
    /// * `spaces` - A slice of space IDs to query for windows.
    /// * `refresh_index` - An integer indicating the refresh index (used internally for tracking).
    ///
    /// # Returns
    ///
    /// `Ok(Vec<Window>)` containing the found and added windows, otherwise `Err(Error)`.
    fn find_existing_application_windows(
        &self,
        app: &mut Application,
        spaces: &[WorkspaceId],
    ) -> Result<(Vec<Window>, Vec<WinID>)>;
    /// Finds the `WinID` of a window at a given screen point.
    ///
    /// # Arguments
    ///
    /// * `point` - A reference to the `CGPoint` representing the screen coordinate.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` with the found window's ID if successful, otherwise `Err(Error)`.
    fn find_window_at_point(&self, point: &CGPoint) -> Result<WinID>;
    /// Returns a list of `WinID`s for all windows in a given workspace (space).
    ///
    /// # Arguments
    ///
    /// * `space_id` - The ID of the space to query.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<WinID>)` containing the list of window IDs, otherwise `Err(Error)`.
    fn windows_in_workspace(&self, space_id: WorkspaceId) -> Result<Vec<WinID>>;
    /// Sends an `Event::Exit` to the event loop, signaling the application to quit.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the exit event is sent successfully, otherwise `Err(Error)`.
    fn quit(&self) -> Result<()>;

    fn setup_config_watcher(&self, path: &Path) -> Result<Box<dyn Watcher>>;
}

/// `WindowManager` is a Bevy resource that holds a boxed `WindowManagerApi` trait object.
/// It allows for dynamic dispatch to the OS-specific window management implementation.
#[derive(Deref, DerefMut, Resource)]
pub struct WindowManager(pub Box<dyn WindowManagerApi>);

/// `WindowManagerOS` is the macOS-specific implementation of the `WindowManagerApi` trait.
/// It directly interacts with the macOS `SkyLight` and Accessibility APIs to manage windows.
pub struct WindowManagerOS {
    main_cid: ConnID,
    event_sender: EventSender,
}

impl WindowManagerOS {
    /// Creates a new `WindowManagerOS` instance.
    /// It initializes the main connection ID to the macOS `SkyLight` API.
    ///
    /// # Arguments
    ///
    /// * `event_sender` - The `EventSender` to dispatch events from the window manager.
    ///
    /// # Returns
    ///
    /// A new `WindowManagerOS` instance.
    pub fn new(event_sender: EventSender) -> Self {
        let main_cid = unsafe { SLSMainConnectionID() };
        debug!("My connection id: {main_cid}");

        Self {
            main_cid,
            event_sender,
        }
    }

    /// Retrieves a list of space IDs for a given display UUID.
    /// It queries the `SkyLight` API for managed display spaces and filters by the provided UUID.
    ///
    /// # Arguments
    ///
    /// * `uuid` - A reference to the `CFString` representing the display's UUID.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<u64>)` with the list of space IDs if successful, otherwise `Err(Error)` if the spaces cannot be retrieved or the display is not found.
    fn display_space_list(&self, uuid: &CFString) -> Result<Vec<WorkspaceId>> {
        let display_spaces = NonNull::new(unsafe { SLSCopyManagedDisplaySpaces(self.main_cid) })
            .map(|ptr| unsafe { CFRetained::from_raw(ptr) })
            .ok_or(Error::PermissionDenied(format!(
                "can not copy managed display spaces for {}.",
                self.main_cid
            )))?;
        let uuid = uuid.to_string();

        let display = display_spaces.iter().find(|display| {
            let identifier = display
                .get(&CFString::from_static_str("Display Identifier"))
                .map(|name| name.to_string());
            identifier.is_some_and(|identifier| {
                // FIXME: Sometimes the main display simply has the name 'Main'.
                identifier == "Main" || identifier == uuid
            })
        });
        let Some(display) = display else {
            return Err(Error::PermissionDenied(format!(
                "could not get any displays for {}",
                self.main_cid
            )));
        };
        debug!("found display with uuid '{uuid}'");

        let display = unsafe {
            display.cast_unchecked::<CFString, CFArray<CFDictionary<CFString, CFNumber>>>()
        };
        let Some(spaces) = display.get(&CFString::from_static_str("Spaces")) else {
            return Err(Error::PermissionDenied(format!(
                "could not get any spaces for dislay '{uuid}'",
            )));
        };

        let spaces = spaces
            .iter()
            .filter_map(|space| {
                space
                    .get(&CFString::from_static_str("id64"))
                    .and_then(|id| id.as_i64().and_then(|value| u64::try_from(value).ok()))
            })
            .collect::<Vec<WorkspaceId>>();
        debug!(
            "spaces [{}]",
            spaces
                .iter()
                .map(|id| format!("{id}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        Ok(spaces)
    }

    /// Retrieves the UUID of the active menu bar display.
    /// This typically corresponds to the display where the primary menu bar is located.
    ///
    /// # Returns
    ///
    /// `Ok(CFRetained<CFString>)` with the UUID if successful, otherwise `Err(Error)` if the active display cannot be determined.
    fn active_display_uuid(&self) -> Result<CFRetained<CFString>> {
        unsafe {
            let ptr = SLSCopyActiveMenuBarDisplayIdentifier(self.main_cid);
            let ptr = NonNull::new(ptr.cast_mut()).ok_or(Error::NotFound(format!(
                "can not find active display for connection {}.",
                self.main_cid
            )))?;
            Ok(CFRetained::from_raw(ptr))
        }
    }

    /// Retrieves the UUID of the display a specific window is currently on.
    /// It first tries `SLSCopyManagedDisplayForWindow` and then falls back to `SLSCopyBestManagedDisplayForRect`
    /// using the window's bounds if the first call fails.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The `WinID` of the window.
    ///
    /// # Returns
    ///
    /// `Ok(CFRetained<CFString>)` with the display UUID if successful, otherwise `Err(Error)` if the display cannot be determined.
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
        .ok_or(Error::InvalidInput(format!(
            "can not get display uuid for window {window_id}.",
        )))
    }

    /// Retrieves the `CGDirectDisplayID` of the display a specific window is currently on.
    /// It internally calls `display_uuid` and then converts the UUID to a display ID.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The `WinID` of the window.
    ///
    /// # Returns
    ///
    /// `Ok(u32)` with the display ID if successful, otherwise `Err(Error)`.
    fn display_id(&self, window_id: WinID) -> Result<CGDirectDisplayID> {
        let uuid = self.display_uuid(window_id);
        uuid.and_then(|uuid| Display::id_from_uuid(&uuid))
    }

    /// Returns the connection ID (`ConnID`) for a given process serial number (`PSN`).
    ///
    /// # Arguments
    ///
    /// * `psn` - The `ProcessSerialNumber` of the process.
    ///
    /// # Returns
    ///
    /// `Some(ConnID)` if the connection ID is found, otherwise `None`.
    fn connection_for_process(&self, psn: ProcessSerialNumber) -> Option<ConnID> {
        let mut connection: ConnID = 0;
        unsafe { SLSGetConnectionIDForPSN(self.main_cid, &psn, &mut connection) };
        (connection != 0).then_some(connection)
    }
}

impl WindowManagerApi for WindowManagerOS {
    fn new_application(&self, process: &dyn ProcessApi) -> Result<Application> {
        let connection = self.connection_for_process(process.psn());
        ApplicationOS::new(connection, process, &self.event_sender)
            .map(|app| Application::new(Box::new(app)))
    }

    /// Returns child windows of the main window.
    fn get_associated_windows(&self, window_id: WinID) -> Vec<WinID> {
        trace!("for window {window_id}");
        let windows =
            unsafe { CFRetained::retain(SLSCopyAssociatedWindows(self.main_cid, window_id)) };
        windows.into_iter().filter_map(|id| id.as_i32()).collect()
    }

    /// Retrieves a list of all currently present displays, along with their associated spaces.
    ///
    /// # Returns
    ///
    /// A `Vec<Self>` containing `Display` objects for all present displays.
    fn present_displays(&self) -> Vec<(Display, Vec<WorkspaceId>)> {
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
            .filter_map(|id| {
                let bounds = CGDisplayBounds(id);
                let mut menubar_height: u32 = 0;
                unsafe { SLSGetDisplayMenubarHeight(id, &raw mut menubar_height) };
                debug!("menubar height: {menubar_height}");
                let workspaces = Display::uuid_from_id(id)
                    .and_then(|uuid| self.display_space_list(uuid.as_ref()))
                    .ok()?;
                Some((
                    Display::new(id, irect_from(bounds), menubar_height.cast_signed()),
                    workspaces,
                ))
            })
            .collect()
    }

    /// Retrieves the `CGDirectDisplayID` of the active menu bar display.
    ///
    /// # Returns
    ///
    /// `Ok(u32)` with the display ID if successful, otherwise `Err(Error)`.
    fn active_display_id(&self) -> Result<u32> {
        let uuid = self.active_display_uuid()?;
        Display::id_from_uuid(&uuid)
    }

    /// Retrieves the ID of the current active space on this display.
    ///
    /// # Returns
    ///
    /// `Ok(u64)` with the space ID if successful, otherwise `Err(Error)`.
    fn active_display_space(&self, display_id: CGDirectDisplayID) -> Result<WorkspaceId> {
        Display::uuid_from_id(display_id).map(|uuid| unsafe {
            SLSManagedDisplayGetCurrentSpace(self.main_cid, &raw const *uuid)
        })
    }

    /// Centers the mouse cursor on the window if it's not already within the window's bounds.
    #[instrument(level = Level::DEBUG, skip_all, fields(window))]
    fn center_mouse(&self, window: Option<&Window>, display_bounds: &IRect) {
        let center = if let Some(window) = window {
            let mut cursor = CGPoint::default();
            if unsafe {
                CGError::Success != SLSGetCurrentCursorLocation(self.main_cid, &mut cursor)
            } {
                warn!("Unable to get current cursor position.");
                return;
            }
            let frame = CGRect::new(
                CGPoint::new(window.frame().min.x.into(), window.frame().min.y.into()),
                CGSize::new(
                    window.frame().width().into(),
                    window.frame().height().into(),
                ),
            );
            if CGRectContainsPoint(frame, cursor) {
                return;
            }

            let center = window.frame().center() + display_bounds.min;
            let display_id = self.display_id(window.id());
            #[allow(clippy::redundant_closure)]
            let bounds = display_id.map(|display_id| CGDisplayBounds(display_id));
            if bounds.is_ok_and(|bounds| !CGRectContainsPoint(bounds, origin_to(center))) {
                return;
            }
            center
        } else {
            display_bounds.center()
        };
        CGWarpMouseCursorPosition(origin_to(center));
    }

    /// Adds existing windows for a given application, attempting to resolve any that are not yet found.
    /// It compares the application's reported window list with the global window list and uses brute-forcing if necessary.
    ///
    /// # Arguments
    ///
    /// * `app` - A mutable reference to the `Application` whose windows are to be added.
    /// * `spaces` - A slice of space IDs to query.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<Window>)` containing the found windows, otherwise `Err(Error)`.
    fn find_existing_application_windows(
        &self,
        app: &mut Application,
        spaces: &[WorkspaceId],
    ) -> Result<(Vec<Window>, Vec<WinID>)> {
        let global_window_list = existing_application_window_list(self.main_cid, app, spaces)?;
        if global_window_list.is_empty() {
            return Err(Error::InvalidInput(format!("No windows found for {app}")));
        }
        debug!("{app} has global windows: {global_window_list:?}");

        let found_windows = app.window_list();
        if found_windows.len() == global_window_list.len() {
            debug!("All windows for {:?} are now resolved", app.psn());
            return Ok((found_windows, vec![]));
        }

        let find_window = |window_id| found_windows.iter().find(|window| window.id() == window_id);
        let offscreen_windows = global_window_list
            .into_iter()
            .filter(|&window_id| find_window(window_id).is_none())
            .collect::<Vec<_>>();
        debug!(
            "{:?} has {} windows that are not yet resolved",
            app.psn(),
            offscreen_windows.len()
        );
        Ok((found_windows, offscreen_windows))
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
    fn find_window_at_point(&self, point: &CGPoint) -> Result<WinID> {
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
        }
        .to_result(function_name!())?;
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
            }
            .to_result(function_name!())?;
        }
        if window_id == 0 {
            Err(Error::invalid_window(&format!(
                "could not find a window at {point:?}",
            )))
        } else {
            Ok(window_id)
        }
    }

    /// Returns a list of windows in a given workspace.
    fn windows_in_workspace(&self, space_id: WorkspaceId) -> Result<Vec<WinID>> {
        space_window_list_for_connection(self.main_cid, &[space_id], None, true)
    }

    fn quit(&self) -> Result<()> {
        self.event_sender.send(Event::Exit)
    }

    fn setup_config_watcher(&self, path: &Path) -> Result<Box<dyn Watcher>> {
        let setup = notify::Config::default()
            .with_poll_interval(Duration::from_secs(3))
            .with_follow_symlinks(false);
        let config_handler = ConfigHandler(self.event_sender.clone());
        let symlink = symlink_target(path);

        let mut watcher = if let Some(symlink) = symlink {
            setup.with_follow_symlinks(true);
            let mut watcher = notify::PollWatcher::new(config_handler, setup)?;
            debug!("watching symlink target {} for changes.", symlink.display());
            watcher.watch(&symlink, RecursiveMode::NonRecursive)?;

            Ok::<Box<dyn Watcher>, Error>(Box::new(watcher))
        } else {
            Ok::<Box<dyn Watcher>, Error>(Box::new(notify::RecommendedWatcher::new(
                config_handler,
                setup,
            )?))
        }?;
        debug!("watching config file {} for changes.", path.display());
        watcher.watch(path, RecursiveMode::NonRecursive)?;
        Ok(watcher)
    }
}

/// Retrieves a list of window IDs for specified spaces and connection, with an option to include minimized windows.
/// This function uses `SkyLight` API calls to query windows based on their space, connection, and visibility tags.
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
    spaces: &[WorkspaceId],
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
    .ok_or(Error::InvalidInput(format!(
        "{}: nullptr returned from SLSCopyWindowsWithOptionsAndTags.",
        function_name!()
    )))?;
    let window_list_ref = unsafe { CFRetained::from_raw(ptr) };

    let count = window_list_ref.count();
    if count == 0 {
        return Err(Error::NotFound(format!(
            "{}: zero windows returned",
            function_name!()
        )));
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

    let mut window_list = Vec::with_capacity(count.try_into()?);
    while unsafe { SLSWindowIteratorAdvance(&raw const *iterator) } {
        let tags = unsafe { SLSWindowIteratorGetTags(&raw const *iterator) };
        let attributes = unsafe { SLSWindowIteratorGetAttributes(&raw const *iterator) };
        let parent_wid: WinID = unsafe { SLSWindowIteratorGetParentID(&raw const *iterator) };
        let window_id: WinID = unsafe { SLSWindowIteratorGetWindowID(&raw const *iterator) };

        trace!(
            "id: {window_id} parent: {parent_wid} tags: 0x{tags:x} attributes: 0x{attributes:x}",
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
/// * `cid` - The main connection ID.
/// * `app` - A reference to the `Application` for which to retrieve window IDs.
/// * `spaces` - A slice of space IDs to query.
///
/// # Returns
///
/// `Ok(Vec<WinID>)` containing the list of window IDs if successful, otherwise `Err(Error)`.
fn existing_application_window_list(
    cid: ConnID,
    app: &Application,
    spaces: &[WorkspaceId],
) -> Result<Vec<WinID>> {
    if spaces.is_empty() {
        return Err(Error::NotFound(format!(
            "{}: no spaces returned",
            function_name!()
        )));
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
pub fn bruteforce_windows(pid: Pid, mut window_list: Vec<WinID>) -> Vec<Window> {
    const MAGIC: u32 = 0x636f_636f;
    const BUFSIZE: isize = 0x14;
    let mut found_windows = Vec::new();
    debug!("{pid} has unresolved window on other desktops, bruteforcing them.");

    //
    // NOTE: MacOS API does not return AXUIElementRef of windows on inactive spaces. However,
    // we can just brute-force the element_id and create the AXUIElementRef ourselves.
    //  https://github.com/decodism
    //  https://github.com/lwouis/alt-tab-macos/issues/1324#issuecomment-2631035482
    //

    let Some(data_ref) = CFMutableData::new(None, BUFSIZE) else {
        error!("error creating mutable data");
        return found_windows;
    };
    CFMutableData::increase_length(data_ref.deref().into(), BUFSIZE);

    let data = unsafe {
        from_raw_parts_mut(
            CFMutableData::mutable_byte_ptr(data_ref.deref().into()),
            BUFSIZE as usize,
        )
    };
    let bytes = pid.to_ne_bytes();
    data[0x0..bytes.len()].copy_from_slice(&bytes);
    let bytes = MAGIC.to_ne_bytes();
    data[0x8..0x8 + bytes.len()].copy_from_slice(&bytes);

    for element_id in 0..0x7fffu64 {
        let bytes = element_id.to_ne_bytes();
        data[0xc..0xc + bytes.len()].copy_from_slice(&bytes);

        let Ok(element_ref) =
            AXUIWrapper::retain(unsafe { _AXUIElementCreateWithRemoteToken(data_ref.as_ref()) })
        else {
            continue;
        };
        let Ok(window_id) = ax_window_id(element_ref.as_ptr()) else {
            continue;
        };

        if let Some(index) = window_list.iter().position(|&id| id == window_id) {
            window_list.remove(index);
            debug!("Found window {window_id:?}");
            if let Ok(window) = WindowOS::new(&element_ref).inspect_err(|err| warn!("{err}")) {
                found_windows.push(Window::new(Box::new(window)));
            }
        }
    }
    found_windows
}

/// Checks if the application has Accessibility privileges.
/// It will prompt the user to grant permission if not already granted.
///
/// # Returns
///
/// `true` if Accessibility privileges are granted, `false` otherwise.
pub fn check_ax_privilege() -> bool {
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt
            .cast::<CFString>()
            .as_ref()
            .unwrap()];
        let values = [kCFBooleanTrue.unwrap()];
        let opts = CFDictionary::from_slices(&keys, &values);
        AXIsProcessTrustedWithOptions((&raw const *opts).cast())
    }
}

/// Checks if the macOS "Displays have separate Spaces" option is enabled.
/// This is crucial for the window manager's functionality, as Paneru relies on independent spaces per display.
///
/// # Returns
///
/// `true` if separate spaces are enabled, `false` otherwise.
pub fn check_separate_spaces() -> bool {
    unsafe {
        let cid = SLSMainConnectionID();
        SLSGetSpaceManagementMode(cid) == 1
    }
}

/// `ConfigHandler` is an implementation of `notify::EventHandler` that reloads the application configuration
/// when the configuration file changes. It also dispatches a `ConfigRefresh` event.
struct ConfigHandler(EventSender);

impl notify::EventHandler for ConfigHandler {
    /// Handles file system events for the configuration file. When the content changes, it reloads the configuration.
    /// Specifically, it responds to `ModifyKind::Data(DataChange::Content)` events.
    ///
    /// # Arguments
    ///
    /// * `event` - The result of a file system event.
    fn handle_event(&mut self, event: notify::Result<notify::Event>) {
        if let Ok(event) = event {
            _ = self.0.send(Event::ConfigRefresh(event)).inspect_err(|err| {
                warn!("error sending config refresh: {err}");
            });
        }
    }
}

use accessibility_sys::{
    AXUIElementRef, AXValueCreate, AXValueGetValue, kAXFloatingWindowSubrole,
    kAXMinimizedAttribute, kAXParentAttribute, kAXPositionAttribute, kAXRaiseAction,
    kAXRoleAttribute, kAXSizeAttribute, kAXStandardWindowSubrole, kAXSubroleAttribute,
    kAXTitleAttribute, kAXUnknownSubrole, kAXValueTypeCGPoint, kAXValueTypeCGSize, kAXWindowRole,
};
use core::ptr::NonNull;
use log::{debug, trace, warn};
use objc2::rc::Retained;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFBooleanGetValue, CFEqual, CFNumber, CFNumberGetType, CFNumberGetValue,
    CFNumberType, CFRetained, CFString, CFType, CFUUIDCreateFromString, CFUUIDCreateString,
    CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGError, CGGetActiveDisplayList, CGRectContainsPoint,
    CGRectEqualToRect, CGWarpMouseCursorPosition,
};
use std::collections::{HashMap, VecDeque};
use std::io::{Error, ErrorKind, Result};
use std::ops::Deref;
use std::ptr::null_mut;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;
use stdext::function_name;
use stdext::prelude::RwLockExt;

use crate::app::Application;
use crate::manager::WindowManager;
use crate::platform::Pid;
use crate::skylight::{
    _AXUIElementGetWindow, _SLPSSetFrontProcessWithOptions, AXUIElementCopyAttributeValue,
    AXUIElementPerformAction, AXUIElementSetAttributeValue, CGDisplayCreateUUIDFromDisplayID,
    CGDisplayGetDisplayIDFromUUID, ConnID, SLPSPostEventRecordTo,
    SLSCopyActiveMenuBarDisplayIdentifier, SLSCopyBestManagedDisplayForRect,
    SLSCopyManagedDisplayForWindow, SLSCopyManagedDisplaySpaces, SLSGetCurrentCursorLocation,
    SLSGetWindowBounds, SLSManagedDisplayGetCurrentSpace, SLSWindowIteratorAdvance,
    SLSWindowIteratorGetCount, SLSWindowIteratorGetParentID, SLSWindowQueryResultCopyWindows,
    SLSWindowQueryWindows, WinID,
};
use crate::util::{
    AxuWrapperType, create_array, get_array_values, get_attribute, get_cfdict_value,
};

#[derive(Clone, Debug)]
pub enum Panel {
    Single(WinID),
    Stack(Vec<WinID>),
}

impl Panel {
    pub fn top(&self) -> Option<WinID> {
        match self {
            Panel::Single(id) => Some(id),
            Panel::Stack(stack) => stack.first(),
        }
        .cloned()
    }
}

#[derive(Clone, Debug, Default)]
pub struct WindowPane {
    pane: Arc<RwLock<VecDeque<Panel>>>,
}

impl WindowPane {
    /// Finds the index of a window within the pane.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to find.
    ///
    /// # Returns
    ///
    /// `Ok(usize)` with the index if found, otherwise `Err(Error)`.
    pub fn index_of(&self, window_id: WinID) -> Result<usize> {
        self.pane
            .force_read()
            .iter()
            .position(|panel| match panel {
                Panel::Single(id) => *id == window_id,
                Panel::Stack(stack) => stack.contains(&window_id),
            })
            .ok_or(Error::new(
                ErrorKind::NotFound,
                format!(
                    "{}: can not find window {window_id} in the current pane.",
                    function_name!()
                ),
            ))
    }

    /// Inserts a window ID into the pane at a specified position.
    ///
    /// # Arguments
    ///
    /// * `after` - The index after which to insert the window.
    /// * `window_id` - The ID of the window to insert.
    ///
    /// # Returns
    ///
    /// `Ok(usize)` with the new index of the inserted window, otherwise `Err(Error)` if the index is out of bounds.
    pub fn insert_at(&self, after: usize, window_id: WinID) -> Result<usize> {
        let index = after + 1;
        if index > self.len() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("{}: index {after} out of bounds.", function_name!()),
            ));
        }
        self.pane
            .force_write()
            .insert(index, Panel::Single(window_id));
        Ok(index)
    }

    /// Appends a window ID to the end of the pane.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to append.
    pub fn append(&self, window_id: WinID) {
        self.pane.force_write().push_back(Panel::Single(window_id));
    }

    /// Removes a window ID from the pane.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to remove.
    pub fn remove(&self, window_id: WinID) {
        let removed = self
            .index_of(window_id)
            .ok()
            .and_then(|index| self.pane.force_write().remove(index).zip(Some(index)));

        if let Some((Panel::Stack(mut stack), index)) = removed {
            stack.retain(|id| *id != window_id);
            if stack.len() > 1 {
                self.pane.force_write().insert(index, Panel::Stack(stack));
            } else {
                self.pane
                    .force_write()
                    .insert(index, Panel::Single(stack[0]));
            }
        }
    }

    /// Retrieves the window ID at a specified index in the pane.
    ///
    /// # Arguments
    ///
    /// * `at` - The index from which to retrieve the window ID.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` with the window ID if the index is valid, otherwise `Err(Error)`.
    pub fn get(&self, at: usize) -> Result<Panel> {
        self.pane.force_read().get(at).cloned().ok_or(Error::new(
            ErrorKind::InvalidInput,
            format!("{}: {at} out of bounds", function_name!()),
        ))
    }

    /// Swaps the positions of two windows within the pane.
    ///
    /// # Arguments
    ///
    /// * `left` - The index of the first window.
    /// * `right` - The index of the second window.
    pub fn swap(&self, left: usize, right: usize) {
        self.pane.force_write().swap(left, right);
    }

    /// Returns the number of windows in the pane.
    ///
    /// # Returns
    ///
    /// The number of windows as `usize`.
    pub fn len(&self) -> usize {
        self.pane.force_read().len()
    }

    /// Returns the ID of the first window in the pane.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` with the first window's ID, otherwise `Err(Error)` if the pane is empty.
    pub fn first(&self) -> Result<Panel> {
        self.pane.force_read().front().cloned().ok_or(Error::new(
            ErrorKind::NotFound,
            format!("{}: can not find first element.", function_name!()),
        ))
    }

    /// Returns the ID of the last window in the pane.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` with the last window's ID, otherwise `Err(Error)` if the pane is empty.
    pub fn last(&self) -> Result<Panel> {
        self.pane.force_read().back().cloned().ok_or(Error::new(
            ErrorKind::NotFound,
            format!("{}: can not find last element.", function_name!()),
        ))
    }

    /// Iterates over windows to the right of a given window, applying an accessor function to each.
    /// Iteration stops if the accessor returns `false`.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the starting window.
    /// * `accessor` - A closure that takes a `WinID` and returns `true` to continue iteration, `false` to stop.
    ///
    /// # Returns
    ///
    /// `Ok(())` if successful, otherwise `Err(Error)` if the starting window is not found.
    pub fn access_right_of(
        &self,
        window_id: WinID,
        mut accessor: impl FnMut(&Panel) -> bool,
    ) -> Result<()> {
        let index = self.index_of(window_id)?;
        for panel in self.pane.force_read().range(1 + index..) {
            if !accessor(panel) {
                break;
            }
        }
        Ok(())
    }

    /// Iterates over windows to the left of a given window (in reverse order), applying an accessor function to each.
    /// Iteration stops if the accessor returns `false`.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the starting window.
    /// * `accessor` - A closure that takes a `WinID` and returns `true` to continue iteration, `false` to stop.
    ///
    /// # Returns
    ///
    /// `Ok(())` if successful, otherwise `Err(Error)` if the starting window is not found.
    pub fn access_left_of(
        &self,
        window_id: WinID,
        mut accessor: impl FnMut(&Panel) -> bool,
    ) -> Result<()> {
        let index = self.index_of(window_id)?;
        for panel in self.pane.force_read().range(0..index).rev() {
            // NOTE: left side iterates backwards.
            if !accessor(panel) {
                break;
            }
        }
        Ok(())
    }

    pub fn stack(&self, window_id: WinID) -> Result<()> {
        let index = self.index_of(window_id)?;
        if index == 0 {
            // Can not stack to the left if left most window already.
            return Ok(());
        }
        if let Panel::Stack(_) = self.pane.force_read()[index] {
            return Ok(());
        }

        self.pane.force_write().remove(index);
        if let Some(panel) = self.pane.force_write().remove(index - 1) {
            let newstack = match panel {
                Panel::Stack(mut stack) => {
                    stack.push(window_id);
                    stack
                }
                Panel::Single(id) => vec![id, window_id],
            };

            debug!("Stacked windows: {:#?}", newstack);
            self.pane
                .force_write()
                .insert(index - 1, Panel::Stack(newstack));
        }

        Ok(())
    }

    pub fn unstack(&self, window_id: WinID) -> Result<()> {
        let index = self.index_of(window_id)?;
        if let Panel::Single(_) = self.pane.force_read()[index] {
            // Can not unstack a single pane
            return Ok(());
        }

        if let Some(panel) = self.pane.force_write().remove(index) {
            let newstack = match panel {
                Panel::Stack(mut stack) => {
                    stack.retain(|id| *id != window_id);
                    if stack.len() == 1 {
                        Panel::Single(stack[0])
                    } else {
                        Panel::Stack(stack)
                    }
                }
                Panel::Single(_) => unreachable!("Is checked at the start of the function"),
            };
            self.pane
                .force_write()
                .insert(index, Panel::Single(window_id));
            self.pane.force_write().insert(index, newstack);
        }

        Ok(())
    }

    pub fn all_windows(&self) -> Vec<WinID> {
        self.pane
            .force_read()
            .iter()
            .flat_map(|panel| match panel {
                Panel::Single(window_id) => vec![*window_id],
                Panel::Stack(ids) => ids.clone(),
            })
            .collect()
    }
}

pub struct Display {
    pub id: CGDirectDisplayID,
    // Map of workspaces, containing panels of windows.
    pub spaces: HashMap<u64, WindowPane>,
    pub bounds: CGRect,
}

impl Display {
    /// Creates a new `Display` instance.
    ///
    /// # Arguments
    ///
    /// * `id` - The `CGDirectDisplayID` of the display.
    /// * `spaces` - A vector of space IDs associated with this display.
    ///
    /// # Returns
    ///
    /// A new `Display` instance.
    fn new(id: CGDirectDisplayID, spaces: Vec<u64>) -> Self {
        let spaces = HashMap::from_iter(spaces.into_iter().map(|id| (id, WindowPane::default())));
        let bounds = unsafe { CGDisplayBounds(id) };
        Self { id, spaces, bounds }
    }

    /// Converts a `CGDirectDisplayID` to a `CFUUID` string.
    ///
    /// # Arguments
    ///
    /// * `id` - The `CGDirectDisplayID` to convert.
    ///
    /// # Returns
    ///
    /// `Ok(CFRetained<CFString>)` with the UUID string if successful, otherwise `Err(Error)`.
    fn uuid_from_id(id: CGDirectDisplayID) -> Result<CFRetained<CFString>> {
        unsafe {
            let uuid = NonNull::new(CGDisplayCreateUUIDFromDisplayID(id))
                .map(|ptr| CFRetained::from_raw(ptr))
                .ok_or(Error::new(
                    ErrorKind::InvalidData,
                    format!("{}: can not create uuid from {id}.", function_name!()),
                ))?;
            CFUUIDCreateString(None, Some(&uuid)).ok_or(Error::new(
                ErrorKind::InvalidData,
                format!("{}: can not create string from {uuid:?}.", function_name!()),
            ))
        }
    }

    /// Converts a `CFUUID` string to a `CGDirectDisplayID`.
    ///
    /// # Arguments
    ///
    /// * `uuid` - The `CFRetained<CFString>` representing the UUID.
    ///
    /// # Returns
    ///
    /// `Ok(u32)` with the `CGDirectDisplayID` if successful, otherwise `Err(Error)`.
    fn id_from_uuid(uuid: CFRetained<CFString>) -> Result<u32> {
        unsafe {
            let id = CFUUIDCreateFromString(None, Some(&uuid)).ok_or(Error::new(
                ErrorKind::NotFound,
                format!("{}: can not convert from {uuid}.", function_name!()),
            ))?;
            Ok(CGDisplayGetDisplayIDFromUUID(id.deref()))
        }
    }

    /// Retrieves a list of space IDs for a given display UUID and connection ID.
    ///
    /// # Arguments
    ///
    /// * `uuid` - A reference to the `CFString` representing the display's UUID.
    /// * `cid` - The connection ID.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<u64>)` with the list of space IDs if successful, otherwise `Err(Error)`.
    fn display_space_list(uuid: &CFString, cid: ConnID) -> Result<Vec<u64>> {
        // let uuid = DisplayManager::display_uuid(display)?;
        unsafe {
            let display_spaces = NonNull::new(SLSCopyManagedDisplaySpaces(cid))
                .map(|ptr| CFRetained::from_raw(ptr))
                .ok_or(Error::new(
                    ErrorKind::PermissionDenied,
                    format!(
                        "{}: can not copy managed display spaces for {cid}.",
                        function_name!()
                    ),
                ))?;

            for display in get_array_values(display_spaces.as_ref()) {
                trace!("{}: display {:?}", function_name!(), display.as_ref());
                let identifier = get_cfdict_value::<CFString>(
                    display.as_ref(),
                    CFString::from_static_str("Display Identifier").deref(),
                )?;
                debug!(
                    "{}: identifier {:?} uuid {:?}",
                    function_name!(),
                    identifier.as_ref(),
                    uuid
                );
                // FIXME: For some reason the main display does not have a UUID in the name, but is
                // referenced as simply "Main".
                if identifier.as_ref().to_string().ne("Main")
                    && !CFEqual(Some(identifier.as_ref()), Some(uuid))
                {
                    continue;
                }

                let spaces = get_cfdict_value::<CFArray>(
                    display.as_ref(),
                    CFString::from_static_str("Spaces").deref(),
                )?;
                debug!("{}: spaces {spaces:?}", function_name!());

                let space_list = get_array_values(spaces.as_ref())
                    .flat_map(|space| {
                        let num = get_cfdict_value::<CFNumber>(
                            space.as_ref(),
                            CFString::from_static_str("id64").deref(),
                        )
                        .ok()?;

                        let mut id = 0u64;
                        CFNumberGetValue(
                            num.as_ref(),
                            CFNumberGetType(num.as_ref()),
                            NonNull::from(&mut id).as_ptr().cast(),
                        );
                        Some(id)
                    })
                    .collect::<Vec<u64>>();
                return Ok(space_list);
            }
        }
        Err(Error::new(
            ErrorKind::NotFound,
            format!("{}: could not get any displays for {cid}", function_name!(),),
        ))
    }

    /// Retrieves a list of all currently present displays, along with their associated spaces.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    ///
    /// # Returns
    ///
    /// A `Vec<Self>` containing `Display` objects for all present displays.
    pub fn present_displays(cid: ConnID) -> Vec<Self> {
        let mut count = 0u32;
        unsafe {
            CGGetActiveDisplayList(0, null_mut(), &mut count);
        }
        if count < 1 {
            return vec![];
        }
        let mut displays = Vec::with_capacity(count.try_into().unwrap());
        unsafe {
            CGGetActiveDisplayList(count, displays.as_mut_ptr(), &mut count);
            displays.set_len(count.try_into().unwrap());
        }
        displays
            .into_iter()
            .flat_map(|id| {
                let uuid = Display::uuid_from_id(id);
                uuid.and_then(|uuid| {
                    Display::display_space_list(uuid.as_ref(), cid)
                        .map(|spaces| Display::new(id, spaces))
                })
            })
            .collect()
    }

    /// Retrieves the UUID of the active menu bar display.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    ///
    /// # Returns
    ///
    /// `Ok(CFRetained<CFString>)` with the UUID if successful, otherwise `Err(Error)`.
    fn active_display_uuid(cid: ConnID) -> Result<CFRetained<CFString>> {
        unsafe {
            let ptr = SLSCopyActiveMenuBarDisplayIdentifier(cid);
            let ptr = NonNull::new(ptr.cast_mut()).ok_or(Error::new(
                ErrorKind::NotFound,
                format!(
                    "{}: can not find active display for connection {cid}.",
                    function_name!(),
                ),
            ))?;
            Ok(CFRetained::from_raw(ptr))
        }
    }

    /// Retrieves the `CGDirectDisplayID` of the active menu bar display.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    ///
    /// # Returns
    ///
    /// `Ok(u32)` with the display ID if successful, otherwise `Err(Error)`.
    pub fn active_display_id(cid: ConnID) -> Result<u32> {
        let uuid = Display::active_display_uuid(cid)?;
        Display::id_from_uuid(uuid)
    }

    /// Retrieves the ID of the current active space on this display.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    ///
    /// # Returns
    ///
    /// `Ok(u64)` with the space ID if successful, otherwise `Err(Error)`.
    fn active_display_space(&self, cid: ConnID) -> Result<u64> {
        Display::uuid_from_id(self.id)
            .map(|uuid| unsafe { SLSManagedDisplayGetCurrentSpace(cid, uuid.deref()) })
    }

    /// Retrieves the `WindowPane` corresponding to the active space on this display.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    ///
    /// # Returns
    ///
    /// `Ok(WindowPane)` if the active panel is found, otherwise `Err(Error)`.
    pub fn active_panel(&self, cid: ConnID) -> Result<WindowPane> {
        let space_id = self.active_display_space(cid)?;
        self.spaces.get(&space_id).cloned().ok_or(Error::new(
            ErrorKind::NotFound,
            format!("{}: can not find space {space_id}.", function_name!()),
        ))
    }

    /// Removes a window from all panes across all spaces on this display.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to remove.
    pub fn remove_window(&self, window_id: WinID) {
        self.spaces.values().for_each(|pane| pane.remove(window_id));
    }
}

/// Retrieves the window ID (WinID) from an `AXUIElementRef`.
///
/// # Arguments
///
/// * `element_ref` - The `AXUIElementRef` to extract the window ID from.
///
/// # Returns
///
/// `Ok(WinID)` with the window ID if successful, otherwise `Err(Error)`.
pub fn ax_window_id(element_ref: AXUIElementRef) -> Result<WinID> {
    let ptr = NonNull::new(element_ref).ok_or(Error::new(
        ErrorKind::InvalidInput,
        format!("{}: nullptr passed as element.", function_name!()),
    ))?;
    let mut window_id: WinID = 0;
    if 0 != unsafe { _AXUIElementGetWindow(ptr.as_ptr(), &mut window_id) } || window_id == 0 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "{}: Unable to get window id from element {element_ref:?}.",
                function_name!()
            ),
        ));
    }
    Ok(window_id)
}

/// Retrieves the process ID (Pid) from an `AxuWrapperType` representing an Accessibility UI element.
///
/// # Arguments
///
/// * `element_ref` - A reference to the `CFRetained<AxuWrapperType>` element.
///
/// # Returns
///
/// `Ok(Pid)` with the process ID if successful, otherwise `Err(Error)`.
pub fn ax_window_pid(element_ref: &CFRetained<AxuWrapperType>) -> Result<Pid> {
    let pid: Pid = unsafe {
        NonNull::new_unchecked(element_ref.as_ptr::<Pid>())
            .byte_add(0x10)
            .read()
    };
    (pid != 0).then_some(pid).ok_or(Error::new(
        ErrorKind::InvalidData,
        format!(
            "{}: can not get pid from {element_ref:?}.",
            function_name!()
        ),
    ))
}

#[derive(Clone)]
pub struct Window {
    inner: Arc<RwLock<InnerWindow>>,
}

pub struct InnerWindow {
    pub id: WinID,
    pub app: Application,
    ax_element: CFRetained<AxuWrapperType>,
    pub frame: CGRect,
    minimized: bool,
    is_root: bool,
    size_ratios: Vec<f64>,
    pub width_ratio: f64,
    pub managed: bool,
}

impl Window {
    /// Creates a new `Window` instance.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window.
    /// * `app` - A reference to the `Application` that owns this window.
    /// * `element_ref` - A `CFRetained<AxuWrapperType>` reference to the Accessibility UI element.
    ///
    /// # Returns
    ///
    /// `Ok(Window)` if the window is created successfully, otherwise `Err(Error)`.
    pub fn new(
        window_id: WinID,
        app: &Application,
        element_ref: CFRetained<AxuWrapperType>,
    ) -> Result<Window> {
        // window->application = application;
        // window->ref = window_ref;
        // window->id = window_id;
        // window->id_ptr = &window->id;
        // window->frame = window_ax_frame(window);
        // window->is_root = !window_parent(window->id) || window_is_root(window);
        // if (window_shadow(window->id)) window_set_flag(window, WINDOW_SHADOW);
        //
        // if (window_is_minimized(window)) {
        //     window_set_flag(window, WINDOW_MINIMIZE);
        // }
        //
        // if ((window_is_fullscreen(window)) ||
        //     (space_is_fullscreen(window_space(window->id)))) {
        //     window_set_flag(window, WINDOW_FULLSCREEN);
        // }
        //
        // if (window_is_sticky(window->id)) {
        //     window_set_flag(window, WINDOW_STICKY);
        // }
        let window = Window {
            inner: Arc::new(RwLock::new(InnerWindow {
                id: window_id,
                app: app.clone(),
                ax_element: element_ref,
                frame: CGRect::default(),
                minimized: false,
                is_root: false,
                size_ratios: vec![0.25, 0.33, 0.50, 0.66, 0.75],
                width_ratio: 0.33,
                managed: true,
            })),
        };

        let minimized = window.is_minimized();
        // window->is_root = !window_parent(window->id) || window_is_root(window);
        let is_root = Window::parent(app.connection()?, window_id).is_err() || window.is_root();
        {
            let mut inner = window.inner.force_write();
            inner.minimized = minimized;
            inner.is_root = is_root;
        }
        Ok(window)
    }

    /// Returns the ID of the window.
    ///
    /// # Returns
    ///
    /// The window ID as `WinID`.
    pub fn id(&self) -> WinID {
        self.inner().id
    }

    /// Returns a clone of the `Application` object that owns this window.
    ///
    /// # Returns
    ///
    /// A cloned `Application` object.
    pub fn app(&self) -> Application {
        self.inner().app.clone()
    }

    /// Returns the current frame (CGRect) of the window.
    ///
    /// # Returns
    ///
    /// The window's frame as `CGRect`.
    pub fn frame(&self) -> CGRect {
        self.inner().frame
    }

    /// Calculates the next preferred size ratio for resizing the window.
    /// It cycles through a predefined set of ratios.
    ///
    /// # Returns
    ///
    /// The next size ratio as `f64`.
    pub fn next_size_ratio(&self) -> f64 {
        let small = *self.inner().size_ratios.first().unwrap();
        let current = self.inner().width_ratio;
        self.inner()
            .size_ratios
            .iter()
            .find(|r| **r > current + 0.05)
            .cloned()
            .unwrap_or(small)
    }

    /// Checks if the window is currently managed by the window manager.
    ///
    /// # Returns
    ///
    /// `true` if the window is managed, `false` otherwise.
    pub fn managed(&self) -> bool {
        self.inner().managed
    }

    /// Sets the managed status of the window.
    ///
    /// # Arguments
    ///
    /// * `manage` - A boolean indicating whether to manage the window.
    pub fn manage(&self, manage: bool) {
        self.inner.force_write().managed = manage;
    }

    /// Returns a read guard to the inner `InnerWindow` for read-only access.
    ///
    /// # Returns
    ///
    /// A `std::sync::RwLockReadGuard` allowing read access to `InnerWindow`.
    pub fn inner(&self) -> std::sync::RwLockReadGuard<'_, InnerWindow> {
        self.inner.force_read()
    }

    /// Returns the raw `AXUIElementRef` of the window's accessibility element.
    ///
    /// # Returns
    ///
    /// The `AXUIElementRef` as `AXUIElementRef`.
    pub fn element(&self) -> AXUIElementRef {
        // unsafe { NonNull::new_unchecked(self.inner().ax_element.as_ptr::<c_void>()).addr() }
        self.inner().ax_element.deref().as_ptr()
    }

    /// Retrieves the parent window ID for a given window.
    ///
    /// # Arguments
    ///
    /// * `main_conn` - The main connection ID.
    /// * `window_id` - The ID of the window whose parent is sought.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` with the parent window ID if successful, otherwise `Err(Error)`.
    pub fn parent(main_conn: ConnID, window_id: WinID) -> Result<WinID> {
        let windows = create_array(vec![window_id], CFNumberType::SInt32Type)?;
        unsafe {
            let query = CFRetained::from_raw(SLSWindowQueryWindows(main_conn, windows.deref(), 1));
            let iterator =
                CFRetained::from_raw(SLSWindowQueryResultCopyWindows(query.deref().into()));
            if 1 == SLSWindowIteratorGetCount(iterator.deref())
                && SLSWindowIteratorAdvance(iterator.deref())
            {
                return Ok(SLSWindowIteratorGetParentID(iterator.deref()));
            }
        }
        Err(Error::new(
            ErrorKind::InvalidInput,
            format!("{}: error creating an array.", function_name!()),
        ))
    }

    /// Retrieves the title of the window.
    ///
    /// # Returns
    ///
    /// `Ok(String)` with the window title if successful, otherwise `Err(Error)`.
    pub fn title(&self) -> Result<String> {
        let axtitle = CFString::from_static_str(kAXTitleAttribute);
        let title = get_attribute::<CFString>(&self.inner().ax_element, axtitle)?;
        Ok(title.to_string())
    }

    /// Retrieves the role of the window (e.g., "AXWindow").
    ///
    /// # Returns
    ///
    /// `Ok(String)` with the window role if successful, otherwise `Err(Error)`.
    pub fn role(&self) -> Result<String> {
        let axrole = CFString::from_static_str(kAXRoleAttribute);
        let role = get_attribute::<CFString>(&self.inner().ax_element, axrole)?;
        Ok(role.to_string())
    }

    /// Retrieves the subrole of the window (e.g., "AXStandardWindow").
    ///
    /// # Returns
    ///
    /// `Ok(String)` with the window subrole if successful, otherwise `Err(Error)`.
    pub fn subrole(&self) -> Result<String> {
        let axrole = CFString::from_static_str(kAXSubroleAttribute);
        let role = get_attribute::<CFString>(&self.inner().ax_element, axrole)?;
        Ok(role.to_string())
    }

    /// Checks if the window's subrole is "AXUnknownSubrole".
    ///
    /// # Returns
    ///
    /// `true` if the subrole is unknown, `false` otherwise.
    pub fn is_unknown(&self) -> bool {
        self.subrole()
            .is_ok_and(|subrole| subrole.eq(kAXUnknownSubrole))
    }

    /// Checks if the window is minimized.
    ///
    /// # Returns
    ///
    /// `true` if the window is minimized, `false` otherwise.
    pub fn is_minimized(&self) -> bool {
        let axminimized = CFString::from_static_str(kAXMinimizedAttribute);
        get_attribute::<CFBoolean>(&self.inner().ax_element, axminimized)
            .map(|minimized| unsafe { CFBooleanGetValue(minimized.deref()) })
            .is_ok_and(|minimized| minimized || self.inner().minimized)
    }

    /// Checks if the window is a root window (i.e., not a child of another window).
    ///
    /// # Returns
    ///
    /// `true` if the window is a root window, `false` otherwise.
    pub fn is_root(&self) -> bool {
        let inner = self.inner();
        let cftype = inner.ax_element.as_ref();
        let axparent = CFString::from_static_str(kAXParentAttribute);
        get_attribute::<CFType>(&self.inner().ax_element, axparent)
            .is_ok_and(|parent| !CFEqual(Some(parent.deref()), Some(cftype)))
    }

    /// Checks if the window is a "real" window based on its role and subrole.
    /// It considers standard and floating window subroles as real.
    ///
    /// # Returns
    ///
    /// `true` if the window is real, `false` otherwise.
    pub fn is_real(&self) -> bool {
        let role = self.role().is_ok_and(|role| role.eq(kAXWindowRole));
        role && self.subrole().is_ok_and(|subrole| {
            [
                kAXStandardWindowSubrole,
                kAXFloatingWindowSubrole,
                // kAXDialogSubrole,
            ]
            .iter()
            .any(|s| subrole.eq(*s))
        })
    }

    /// Checks if the window is eligible for management (i.e., it is a root and a real window).
    ///
    /// # Returns
    ///
    /// `true` if the window is eligible, `false` otherwise.
    pub fn is_eligible(&self) -> bool {
        let me = self.inner();
        me.is_root && self.is_real() // TODO: check for WINDOW_RULE_MANAGED
    }

    /// Repositions the window to the specified x and y coordinates.
    ///
    /// # Arguments
    ///
    /// * `x` - The new x-coordinate for the window's origin.
    /// * `y` - The new y-coordinate for the window's origin.
    pub fn reposition(&self, x: f64, y: f64) {
        let mut point = CGPoint::new(x, y);
        let position_ref = unsafe {
            AXValueCreate(
                kAXValueTypeCGPoint,
                NonNull::from(&mut point).as_ptr().cast(),
            )
        };
        if let Ok(position) = AxuWrapperType::retain(position_ref) {
            unsafe {
                AXUIElementSetAttributeValue(
                    self.inner().ax_element.as_ptr(),
                    CFString::from_static_str(kAXPositionAttribute).as_ref(),
                    position.as_ref(),
                )
            };
            self.inner.force_write().frame.origin = point;
        }
    }

    /// Resizes the window to the specified width and height. It also updates the `width_ratio`.
    ///
    /// # Arguments
    ///
    /// * `width` - The new width of the window.
    /// * `height` - The new height of the window.
    /// * `display_bounds` - The `CGRect` representing the bounds of the display the window is on.
    pub fn resize(&self, width: f64, height: f64, display_bounds: &CGRect) {
        let mut size = CGSize::new(width, height);
        let size_ref =
            unsafe { AXValueCreate(kAXValueTypeCGSize, NonNull::from(&mut size).as_ptr().cast()) };
        if let Ok(position) = AxuWrapperType::retain(size_ref) {
            unsafe {
                AXUIElementSetAttributeValue(
                    self.inner().ax_element.as_ptr(),
                    CFString::from_static_str(kAXSizeAttribute).as_ref(),
                    position.as_ref(),
                )
            };
            let mut inner = self.inner.force_write();
            inner.frame.size = size;
            inner.width_ratio = size.width / display_bounds.size.width;
        }
    }

    /// Updates the internal `frame` of the window by querying its current position and size from the Accessibility API.
    /// It also updates the `width_ratio`.
    ///
    /// # Arguments
    ///
    /// * `display_bounds` - The `CGRect` representing the bounds of the display the window is on.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the frame is updated successfully, otherwise `Err(Error)`.
    pub fn update_frame(&self, display_bounds: &CGRect) -> Result<()> {
        let window_ref = self.inner().ax_element.as_ptr();

        let position = unsafe {
            let mut position_ref: *mut CFType = null_mut();
            AXUIElementCopyAttributeValue(
                window_ref,
                CFString::from_static_str(kAXPositionAttribute).as_ref(),
                &mut position_ref,
            );
            AxuWrapperType::retain(position_ref)?
        };
        let size = unsafe {
            let mut size_ref: *mut CFType = null_mut();
            AXUIElementCopyAttributeValue(
                window_ref,
                CFString::from_static_str(kAXSizeAttribute).as_ref(),
                &mut size_ref,
            );
            AxuWrapperType::retain(size_ref)?
        };

        let mut frame = CGRect::default();
        unsafe {
            AXValueGetValue(
                position.as_ptr(),
                kAXValueTypeCGPoint,
                NonNull::from(&mut frame.origin).as_ptr().cast(),
            );
            AXValueGetValue(
                size.as_ptr(),
                kAXValueTypeCGSize,
                NonNull::from(&mut frame.size).as_ptr().cast(),
            );
        }
        if unsafe { !CGRectEqualToRect(frame, self.inner().frame) } {
            let mut inner = self.inner.force_write();
            inner.frame = frame;
            inner.width_ratio = frame.size.width / display_bounds.size.width;
        }
        Ok(())
    }

    /// Makes the window the key window for its application by sending synthesized events.
    fn make_key_window(&self) {
        let psn = self.app().psn().unwrap();
        let window_id = self.id();
        //
        // :SynthesizedEvent
        //
        // NOTE(koekeishiya): These events will be picked up by an event-tap registered at the
        // "Annotated Session" location; specifying that an event-tap is placed at the point where
        // session events have been annotated to flow to an application.

        let mut event_bytes = [0u8; 0xf8];
        event_bytes[0x04] = 0xf8;
        event_bytes[0x3a] = 0x10;
        let wid = window_id.to_ne_bytes();
        event_bytes[0x3c..(0x3c + wid.len())].copy_from_slice(&wid);
        event_bytes[0x20..(0x20 + 0x10)]
            .iter_mut()
            .for_each(|b| *b = 0xff);

        event_bytes[0x08] = 0x01;
        unsafe { SLPSPostEventRecordTo(&psn, event_bytes.as_ptr().cast()) };

        event_bytes[0x08] = 0x02;
        unsafe { SLPSPostEventRecordTo(&psn, event_bytes.as_ptr().cast()) };
    }

    // const CPS_ALL_WINDOWS: u32 = 0x100;
    const CPS_USER_GENERATED: u32 = 0x200;
    // const CPS_NO_WINDOWS: u32 = 0x400;

    /// Focuses the window without raising it. This involves sending specific events to the process.
    ///
    /// # Arguments
    ///
    /// * `window_manager` - A reference to the `WindowManager` to access focused window information.
    pub fn focus_without_raise(&self, window_manager: &WindowManager) {
        let psn = self.app().psn().unwrap();
        let window_id = self.id();
        debug!("{}: {window_id}", function_name!());
        if window_manager.focused_psn == psn && window_manager.focused_window.is_some() {
            let mut event_bytes = [0u8; 0xf8];
            event_bytes[0x04] = 0xf8;
            event_bytes[0x08] = 0x0d;

            event_bytes[0x8a] = 0x02;
            let wid = window_manager.focused_window.unwrap().to_ne_bytes();
            event_bytes[0x3c..(0x3c + wid.len())].copy_from_slice(&wid);
            unsafe {
                SLPSPostEventRecordTo(&window_manager.focused_psn, event_bytes.as_ptr().cast());
            }
            // @hack
            // Artificially delay the activation by 1ms. This is necessary because some
            // applications appear to be confused if both of the events appear instantaneously.
            thread::sleep(Duration::from_millis(20));

            event_bytes[0x8a] = 0x01;
            let wid = &window_id.to_ne_bytes();
            event_bytes[0x3c..(0x3c + wid.len())].copy_from_slice(wid);
            unsafe {
                SLPSPostEventRecordTo(&psn, event_bytes.as_ptr().cast());
            }
        }

        unsafe {
            _SLPSSetFrontProcessWithOptions(&psn, window_id, Self::CPS_USER_GENERATED);
        }
        self.make_key_window();
    }

    /// Focuses the window and raises it to the front.
    pub fn focus_with_raise(&self) {
        let psn = self.app().psn().unwrap();
        let window_id = self.id();
        unsafe {
            _SLPSSetFrontProcessWithOptions(&psn, window_id, Self::CPS_USER_GENERATED);
        }
        self.make_key_window();
        let element_ref = self.inner().ax_element.as_ptr();
        let action = CFString::from_static_str(kAXRaiseAction);
        unsafe { AXUIElementPerformAction(element_ref, &action) };
    }

    /// Retrieves the UUID of the display the window is currently on.
    /// It first tries `SLSCopyManagedDisplayForWindow` and then falls back to `SLSCopyBestManagedDisplayForRect`.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    ///
    /// # Returns
    ///
    /// `Ok(Retained<CFString>)` with the display UUID if successful, otherwise `Err(Error)`.
    fn display_uuid(&self, cid: ConnID) -> Result<Retained<CFString>> {
        let window_id = self.id();
        let uuid = unsafe {
            NonNull::new(SLSCopyManagedDisplayForWindow(cid, window_id).cast_mut())
                .and_then(|uuid| Retained::from_raw(uuid.as_ptr()))
        };
        uuid.or_else(|| {
            let mut frame = CGRect::default();
            unsafe {
                SLSGetWindowBounds(cid, window_id, &mut frame);
                NonNull::new(SLSCopyBestManagedDisplayForRect(cid, frame).cast_mut())
                    .and_then(|uuid| Retained::from_raw(uuid.as_ptr()))
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
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    ///
    /// # Returns
    ///
    /// `Ok(u32)` with the display ID if successful, otherwise `Err(Error)`.
    fn display_id(&self, cid: ConnID) -> Result<u32> {
        let uuid = self.display_uuid(cid);
        uuid.and_then(|uuid| Display::id_from_uuid(uuid.into()))
    }

    /// Checks if the window is fully visible within the given display bounds.
    ///
    /// # Arguments
    ///
    /// * `display_bounds` - The `CGRect` representing the bounds of the display.
    ///
    /// # Returns
    ///
    /// `true` if the window is fully visible, `false` otherwise.
    pub fn fully_visible(&self, display_bounds: &CGRect) -> bool {
        let frame = self.inner().frame;
        frame.origin.x > 0.0 && frame.origin.x < display_bounds.size.width - frame.size.width
    }

    /// Centers the mouse cursor on the window if it's not already within the window's bounds.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    pub fn center_mouse(&self, cid: ConnID) {
        // TODO: check for MouseFollowsFocus setting in WindowManager and also whether it's
        // overriden for individual window.

        let frame = self.inner().frame;
        let mut cursor = CGPoint::default();
        if unsafe { CGError::Success != SLSGetCurrentCursorLocation(cid, &mut cursor) } {
            warn!(
                "{}: Unable to get current cursor position.",
                function_name!()
            );
            return;
        }
        if unsafe { CGRectContainsPoint(frame, cursor) } {
            return;
        }

        let center = CGPoint::new(
            frame.origin.x + frame.size.width / 2.0,
            frame.origin.y + frame.size.height / 2.0,
        );
        let display_id = self.display_id(cid);
        let bounds = display_id.map(|display_id| unsafe { CGDisplayBounds(display_id) });
        if bounds.is_ok_and(|bounds| unsafe { !CGRectContainsPoint(bounds, center) }) {
            return;
        }
        unsafe { CGWarpMouseCursorPosition(center) };
    }

    /// Adjusts the window's position to ensure it is fully exposed (visible on screen) within the display bounds.
    ///
    /// # Arguments
    ///
    /// * `display_bounds` - The `CGRect` representing the bounds of the display.
    ///
    /// # Returns
    ///
    /// The adjusted `CGRect` of the window after exposure.
    pub fn expose_window(&self, display_bounds: &CGRect) -> CGRect {
        // Check if window needs to be fully exposed
        let window_id = self.id();
        let mut frame = self.inner().frame;
        trace!("{}: focus original position {frame:?}", function_name!());
        let moved = if frame.origin.x + frame.size.width > display_bounds.size.width {
            trace!(
                "{}: Bumped window {} to the left",
                function_name!(),
                window_id
            );
            frame.origin.x = display_bounds.size.width - frame.size.width;
            true
        } else if frame.origin.x < 0.0 {
            trace!(
                "{}: Bumped window {} to the right",
                function_name!(),
                window_id
            );
            frame.origin.x = 0.0;
            true
        } else {
            false
        };
        if moved {
            self.reposition(frame.origin.x, frame.origin.y);
            trace!("{}: focus resposition to {frame:?}", function_name!());
        }
        frame
    }
}

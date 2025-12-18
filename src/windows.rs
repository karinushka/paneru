use accessibility_sys::{
    AXUIElementRef, AXValueCreate, AXValueGetValue, kAXFloatingWindowSubrole,
    kAXMinimizedAttribute, kAXParentAttribute, kAXPositionAttribute, kAXRaiseAction,
    kAXRoleAttribute, kAXSizeAttribute, kAXStandardWindowSubrole, kAXSubroleAttribute,
    kAXTitleAttribute, kAXUnknownSubrole, kAXValueTypeCGPoint, kAXValueTypeCGSize, kAXWindowRole,
};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::system::Commands;
use core::ptr::NonNull;
use log::{debug, trace, warn};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFEqual, CFNumber, CFNumberType, CFRetained, CFString, CFType, CFUUID,
    CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGError, CGGetActiveDisplayList, CGRectContainsPoint,
    CGRectEqualToRect, CGWarpMouseCursorPosition,
};
use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::ops::Deref;
use std::ptr::null_mut;
use std::thread;
use std::time::Duration;
use stdext::function_name;

use crate::errors::{Error, Result};
use crate::events::RepositionMarker;
use crate::platform::{Pid, ProcessSerialNumber};
use crate::skylight::{
    _AXUIElementGetWindow, _SLPSSetFrontProcessWithOptions, AXUIElementCopyAttributeValue,
    AXUIElementPerformAction, AXUIElementSetAttributeValue, CGDisplayCreateUUIDFromDisplayID,
    CGDisplayGetDisplayIDFromUUID, ConnID, SLPSPostEventRecordTo,
    SLSCopyActiveMenuBarDisplayIdentifier, SLSCopyBestManagedDisplayForRect,
    SLSCopyManagedDisplayForWindow, SLSCopyManagedDisplaySpaces, SLSGetCurrentCursorLocation,
    SLSGetDisplayMenubarHeight, SLSGetWindowBounds, SLSManagedDisplayGetCurrentSpace,
    SLSWindowIteratorAdvance, SLSWindowIteratorGetCount, SLSWindowIteratorGetParentID,
    SLSWindowQueryResultCopyWindows, SLSWindowQueryWindows, WinID,
};
use crate::util::{AXUIWrapper, create_array, get_array_values, get_attribute, get_cfdict_value};

#[derive(Clone, Debug)]
pub enum Panel {
    Single(Entity),
    Stack(Vec<Entity>),
}

impl Panel {
    /// Returns the top window ID in the panel.
    pub fn top(&self) -> Option<Entity> {
        match self {
            Panel::Single(id) => Some(id),
            Panel::Stack(stack) => stack.first(),
        }
        .copied()
    }
}

#[derive(Debug, Default)]
pub struct WindowPane {
    pane: VecDeque<Panel>,
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
    pub fn index_of(&self, window_id: Entity) -> Result<usize> {
        self.pane
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
    /// If the index is out of bounds, it will simply append at the end.
    pub fn insert_at(&mut self, after: usize, window_id: Entity) {
        let index = after;
        if index >= self.len() {
            self.pane.push_back(Panel::Single(window_id));
        }
        self.pane.insert(index, Panel::Single(window_id));
    }

    /// Appends a window ID to the end of the pane.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to append.
    pub fn append(&mut self, window_id: Entity) {
        self.pane.push_back(Panel::Single(window_id));
    }

    /// Removes a window ID from the pane.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to remove.
    pub fn remove(&mut self, window_id: Entity) {
        let removed = self
            .index_of(window_id)
            .ok()
            .and_then(|index| self.pane.remove(index).zip(Some(index)));

        if let Some((Panel::Stack(mut stack), index)) = removed {
            stack.retain(|id| *id != window_id);
            if stack.len() > 1 {
                self.pane.insert(index, Panel::Stack(stack));
            } else {
                self.pane.insert(index, Panel::Single(stack[0]));
            }
        }
    }

    /// Retrieves the window panel at a specified index in the pane.
    ///
    /// # Arguments
    ///
    /// * `at` - The index from which to retrieve the window panel.
    ///
    /// # Returns
    ///
    /// `Ok(Panel)` with the window panel if the index is valid, otherwise `Err(Error)`.
    pub fn get(&self, at: usize) -> Result<Panel> {
        self.pane.get(at).cloned().ok_or(Error::new(
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
    pub fn swap(&mut self, left: usize, right: usize) {
        self.pane.swap(left, right);
    }

    /// Returns the number of windows in the pane.
    ///
    /// # Returns
    ///
    /// The number of windows as `usize`.
    pub fn len(&self) -> usize {
        self.pane.len()
    }

    /// Returns the first panel in the pane.
    ///
    /// # Returns
    ///
    /// `Ok(Panel)` with the first panel, otherwise `Err(Error)` if the pane is empty.
    pub fn first(&self) -> Result<Panel> {
        self.pane.front().cloned().ok_or(Error::new(
            ErrorKind::NotFound,
            format!("{}: can not find first element.", function_name!()),
        ))
    }

    /// Returns the last panel in the pane.
    ///
    /// # Returns
    ///
    /// `Ok(Panel)` with the last panel, otherwise `Err(Error)` if the pane is empty.
    pub fn last(&self) -> Result<Panel> {
        self.pane.back().cloned().ok_or(Error::new(
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
    /// * `accessor` - A closure that takes a `&Panel` and returns `true` to continue iteration, `false` to stop.
    ///
    /// # Returns
    ///
    /// `Ok(())` if successful, otherwise `Err(Error)` if the starting window is not found.
    pub fn access_right_of(
        &self,
        window_id: Entity,
        mut accessor: impl FnMut(&Panel) -> bool,
    ) -> Result<()> {
        let index = self.index_of(window_id)?;
        for panel in self.pane.range(1 + index..) {
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
    /// * `accessor` - A closure that takes a `&Panel` and returns `true` to continue iteration, `false` to stop.
    ///
    /// # Returns
    ///
    /// `Ok(())` if successful, otherwise `Err(Error)` if the starting window is not found.
    pub fn access_left_of(
        &self,
        window_id: Entity,
        mut accessor: impl FnMut(&Panel) -> bool,
    ) -> Result<()> {
        let index = self.index_of(window_id)?;
        for panel in self.pane.range(0..index).rev() {
            // NOTE: left side iterates backwards.
            if !accessor(panel) {
                break;
            }
        }
        Ok(())
    }

    /// Stacks the window with the given ID onto the panel to its left.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to stack.
    pub fn stack(&mut self, window_id: Entity) -> Result<()> {
        let index = self.index_of(window_id)?;
        if index == 0 {
            // Can not stack to the left if left most window already.
            return Ok(());
        }
        if let Panel::Stack(_) = self.pane[index] {
            return Ok(());
        }

        self.pane.remove(index);
        let panel = self.pane.remove(index - 1);
        if let Some(panel) = panel {
            let newstack = match panel {
                Panel::Stack(mut stack) => {
                    stack.push(window_id);
                    stack
                }
                Panel::Single(id) => vec![id, window_id],
            };

            debug!("Stacked windows: {newstack:#?}");
            self.pane.insert(index - 1, Panel::Stack(newstack));
        }

        Ok(())
    }

    /// Unstacks the window with the given ID from its current stack.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to unstack.
    pub fn unstack(&mut self, window_id: Entity) -> Result<()> {
        let index = self.index_of(window_id)?;
        if let Panel::Single(_) = self.pane[index] {
            // Can not unstack a single pane
            return Ok(());
        }

        let panel = self.pane.remove(index);
        if let Some(panel) = panel {
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
            self.pane.insert(index, Panel::Single(window_id));
            self.pane.insert(index, newstack);
        }

        Ok(())
    }

    /// Returns a vector of all window IDs in the pane.
    pub fn all_windows(&self) -> Vec<Entity> {
        self.pane
            .iter()
            .flat_map(|panel| match panel {
                Panel::Single(window_id) => vec![*window_id],
                Panel::Stack(ids) => ids.clone(),
            })
            .collect()
    }
}

impl std::fmt::Display for WindowPane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let out = self
            .pane
            .iter()
            .map(|panel| format!("{panel:?}"))
            .collect::<Vec<_>>();
        write!(f, "[{}]", out.join(", "))
    }
}

#[derive(Component)]
pub struct Display {
    pub id: CGDirectDisplayID,
    // Map of workspaces, containing panels of windows.
    pub spaces: HashMap<u64, WindowPane>,
    pub bounds: CGRect,
    pub menubar_height: f64,
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
        let spaces = spaces
            .into_iter()
            .map(|id| (id, WindowPane::default()))
            .collect::<HashMap<_, _>>();
        let bounds = CGDisplayBounds(id);
        let mut menubar_height: u32 = 0;
        unsafe { SLSGetDisplayMenubarHeight(id, &raw mut menubar_height) };
        debug!("{}: menubar height: {menubar_height}", function_name!());

        Self {
            id,
            spaces,
            bounds,
            menubar_height: menubar_height.into(),
        }
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
            CFUUID::new_string(None, Some(&uuid)).ok_or(Error::new(
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
    fn id_from_uuid(uuid: &CFRetained<CFString>) -> Result<u32> {
        unsafe {
            let id = CFUUID::from_string(None, Some(uuid)).ok_or(Error::new(
                ErrorKind::NotFound,
                format!("{}: can not convert from {uuid}.", function_name!()),
            ))?;
            Ok(CGDisplayGetDisplayIDFromUUID(&id))
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
        let display_spaces = NonNull::new(unsafe { SLSCopyManagedDisplaySpaces(cid) })
            .map(|ptr| unsafe { CFRetained::from_raw(ptr) })
            .ok_or(Error::new(
                ErrorKind::PermissionDenied,
                format!(
                    "{}: can not copy managed display spaces for {cid}.",
                    function_name!()
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
        Display::id_from_uuid(&uuid)
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
    pub fn active_display_space(&self, cid: ConnID) -> Result<u64> {
        Display::uuid_from_id(self.id)
            .map(|uuid| unsafe { SLSManagedDisplayGetCurrentSpace(cid, &raw const *uuid) })
    }

    /// Retrieves the `WindowPane` corresponding to the active space on this display.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    ///
    /// # Returns
    ///
    /// `Ok(&mut WindowPane)` if the active panel is found, otherwise `Err(Error)`.
    pub fn active_panel(&mut self, cid: ConnID) -> Result<&mut WindowPane> {
        let space_id = self.active_display_space(cid)?;
        self.spaces.get_mut(&space_id).ok_or(Error::new(
            ErrorKind::NotFound,
            format!("{}: can not find space {space_id}.", function_name!()),
        ))
    }

    /// Removes a window from all panes across all spaces on this display.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to remove.
    pub fn remove_window(&mut self, window_id: Entity) {
        self.spaces
            .values_mut()
            .for_each(|pane| pane.remove(window_id));
    }
}

/// Retrieves the window ID (`WinID`) from an `AXUIElementRef`.
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

/// Retrieves the process ID (Pid) from an `AXUIWrapper` representing an Accessibility UI element.
///
/// # Arguments
///
/// * `element_ref` - A reference to the `CFRetained<AXUIWrapper>` element.
///
/// # Returns
///
/// `Ok(Pid)` with the process ID if successful, otherwise `Err(Error)`.
pub fn ax_window_pid(element_ref: &CFRetained<AXUIWrapper>) -> Result<Pid> {
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

#[derive(Component, Debug)]
pub struct Window {
    id: WinID,
    pub psn: Option<ProcessSerialNumber>,
    ax_element: CFRetained<AXUIWrapper>,
    pub frame: CGRect,
    pub minimized: bool,
    pub eligible: bool,
    pub width_ratio: f64,
    managed: bool,
}

impl Window {
    /// Creates a new `Window` instance.
    ///
    /// # Arguments
    ///
    /// * `element` - A `CFRetained<AXUIWrapper>` reference to the Accessibility UI element.
    ///
    /// # Returns
    ///
    /// `Ok(Window)` if the window is created successfully, otherwise `Err(Error)`.
    pub fn new(element: &CFRetained<AXUIWrapper>) -> Result<Window> {
        let id = ax_window_id(element.as_ptr())?;
        let mut window = Self {
            id,
            psn: None,
            ax_element: element.clone(),
            frame: CGRect::default(),
            minimized: false,
            eligible: false,
            width_ratio: 0.33,
            managed: true,
        };

        if window.is_unknown() {
            return Err(Error::invalid_window(&format!(
                "{}: Ignoring AXUnknown window, id: {}",
                function_name!(),
                window.id()
            )));
        }

        if !window.is_real() {
            return Err(Error::invalid_window(&format!(
                "{}: Ignoring non-real window, id: {}",
                function_name!(),
                window.id()
            )));
        }

        window.minimized = window.is_minimized();
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

    /// Returns the ID of the window.
    ///
    /// # Returns
    ///
    /// The window ID as `WinID`.
    pub fn id(&self) -> WinID {
        self.id
    }

    /// Returns the process serial number of the window.
    pub fn psn(&self) -> Option<ProcessSerialNumber> {
        self.psn
    }

    /// Returns the current frame (`CGRect`) of the window.
    ///
    /// # Returns
    ///
    /// The window's frame as `CGRect`.
    pub fn frame(&self) -> CGRect {
        self.frame
    }

    /// Calculates the next preferred size ratio for resizing the window.
    /// It cycles through a predefined set of ratios.
    ///
    /// # Arguments
    ///
    /// * `size_ratios` - A slice of `f64` representing the preset size ratios.
    ///
    /// # Returns
    ///
    /// The next size ratio as `f64`.
    pub fn next_size_ratio(&self, size_ratios: &[f64]) -> f64 {
        let current = self.width_ratio;
        size_ratios
            .iter()
            .find(|r| **r > current + 0.05)
            .copied()
            .unwrap_or_else(|| *size_ratios.first().unwrap())
    }

    /// Checks if the window is currently managed by the window manager.
    ///
    /// # Returns
    ///
    /// `true` if the window is managed, `false` otherwise.
    pub fn managed(&self) -> bool {
        self.managed
    }

    /// Sets the managed status of the window.
    ///
    /// # Arguments
    ///
    /// * `manage` - A boolean indicating whether to manage the window.
    pub fn manage(&mut self, manage: bool) {
        self.managed = manage;
    }

    /// Returns the accessibility element of the window.
    ///
    /// # Returns
    ///
    /// A `CFRetained<AXUIWrapper>` representing the accessibility element.
    pub fn element(&self) -> CFRetained<AXUIWrapper> {
        self.ax_element.clone()
    }

    /// Retrieves the parent window ID for of the window.
    ///
    /// # Arguments
    ///
    /// * `main_conn` - The main connection ID.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` with the parent window ID if successful, otherwise `Err(Error)`.
    pub fn parent(&self, main_conn: ConnID) -> Result<WinID> {
        let windows = create_array(&[self.id], CFNumberType::SInt32Type)?;
        unsafe {
            let query =
                CFRetained::from_raw(SLSWindowQueryWindows(main_conn, &raw const *windows, 1));
            let iterator = &raw const *CFRetained::from_raw(SLSWindowQueryResultCopyWindows(
                query.deref().into(),
            ));
            if 1 == SLSWindowIteratorGetCount(iterator) && SLSWindowIteratorAdvance(iterator) {
                return Ok(SLSWindowIteratorGetParentID(iterator));
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
        let title = get_attribute::<CFString>(&self.ax_element, &axtitle)?;
        Ok(title.to_string())
    }

    /// Returns true if the window has a valid role.
    pub fn valid_role(&self) -> Result<bool> {
        let role = self.role()?;
        Ok(["AXSheet", "AXDrawer"]
            .iter()
            .any(|axrole| axrole.eq(&role)))
    }

    /// Retrieves the role of the window (e.g., "`AXWindow`").
    ///
    /// # Returns
    ///
    /// `Ok(String)` with the window role if successful, otherwise `Err(Error)`.
    pub fn role(&self) -> Result<String> {
        let axrole = CFString::from_static_str(kAXRoleAttribute);
        let role = get_attribute::<CFString>(&self.ax_element, &axrole)?;
        Ok(role.to_string())
    }

    /// Retrieves the subrole of the window (e.g., "`AXStandardWindow`").
    ///
    /// # Returns
    ///
    /// `Ok(String)` with the window subrole if successful, otherwise `Err(Error)`.
    pub fn subrole(&self) -> Result<String> {
        let axrole = CFString::from_static_str(kAXSubroleAttribute);
        let role = get_attribute::<CFString>(&self.ax_element, &axrole)?;
        Ok(role.to_string())
    }

    /// Checks if the window's subrole is "`AXUnknownSubrole`".
    ///
    /// # Returns
    ///
    /// `true` if the subrole is unknown, `false` otherwise.
    fn is_unknown(&self) -> bool {
        self.subrole()
            .is_ok_and(|subrole| subrole.eq(kAXUnknownSubrole))
    }

    /// Checks if the window is minimized.
    ///
    /// # Returns
    ///
    /// `true` if the window is minimized, `false` otherwise.
    fn is_minimized(&self) -> bool {
        let axminimized = CFString::from_static_str(kAXMinimizedAttribute);
        get_attribute::<CFBoolean>(&self.ax_element, &axminimized)
            .map(|minimized| CFBoolean::value(&minimized))
            .is_ok_and(|minimized| minimized)
    }

    /// Checks if the window is a root window (i.e., not a child of another window).
    ///
    /// # Returns
    ///
    /// `true` if the window is a root window, `false` otherwise.
    pub fn is_root(&self) -> bool {
        let cftype = self.ax_element.as_ref();
        let axparent = CFString::from_static_str(kAXParentAttribute);
        get_attribute::<CFType>(&self.ax_element, &axparent)
            .is_ok_and(|parent| !CFEqual(Some(&*parent), Some(cftype)))
    }

    /// Checks if the window is a "real" window based on its role and subrole.
    /// It considers standard and floating window subroles as real.
    ///
    /// # Returns
    ///
    /// `true` if the window is real, `false` otherwise.
    fn is_real(&self) -> bool {
        let role = self.role().ok();
        let subrole = self.subrole().ok();

        subrole.as_deref() == Some(kAXStandardWindowSubrole)
            || (role.as_deref() == Some(kAXWindowRole)
                && subrole.as_deref() == Some(kAXFloatingWindowSubrole))
    }

    /// Checks if the window is eligible for management (i.e., it is a root and a real window).
    ///
    /// # Returns
    ///
    /// `true` if the window is eligible, `false` otherwise.
    pub fn is_eligible(&self) -> bool {
        self.eligible
    }

    /// Repositions the window to the specified x and y coordinates.
    ///
    /// # Arguments
    ///
    /// * `x` - The new x-coordinate for the window's origin.
    /// * `y` - The new y-coordinate for the window's origin.
    /// * `display_bounds` - The `CGRect` of the display.
    pub fn reposition(&mut self, x: f64, y: f64, display_bounds: &CGRect) {
        if (self.frame.origin.x - x).abs() < 0.1 && (self.frame.origin.y - y).abs() < 0.1 {
            trace!("{}: already in position.", function_name!());
            return;
        }
        let mut point = CGPoint::new(x + display_bounds.origin.x, y + display_bounds.origin.y);
        let position_ref = unsafe {
            AXValueCreate(
                kAXValueTypeCGPoint,
                NonNull::from(&mut point).as_ptr().cast(),
            )
        };
        if let Ok(position) = AXUIWrapper::retain(position_ref) {
            unsafe {
                AXUIElementSetAttributeValue(
                    self.ax_element.as_ptr(),
                    CFString::from_static_str(kAXPositionAttribute).as_ref(),
                    position.as_ref(),
                )
            };
            self.frame.origin.x = x;
            self.frame.origin.y = y;
        }
    }

    /// Resizes the window to the specified width and height. It also updates the `width_ratio`.
    ///
    /// # Arguments
    ///
    /// * `width` - The new width of the window.
    /// * `height` - The new height of the window.
    /// * `display_bounds` - The `CGRect` representing the bounds of the display the window is on.
    pub fn resize(&mut self, width: f64, height: f64, display_bounds: &CGRect) {
        if (self.frame.size.width - width).abs() < 0.1
            && (self.frame.size.height - height).abs() < 0.1
        {
            trace!("{}: already correct size.", function_name!());
            return;
        }
        let mut size = CGSize::new(width, height);
        let size_ref =
            unsafe { AXValueCreate(kAXValueTypeCGSize, NonNull::from(&mut size).as_ptr().cast()) };
        if let Ok(position) = AXUIWrapper::retain(size_ref) {
            unsafe {
                AXUIElementSetAttributeValue(
                    self.ax_element.as_ptr(),
                    CFString::from_static_str(kAXSizeAttribute).as_ref(),
                    position.as_ref(),
                )
            };
            self.frame.size = size;
            self.width_ratio = size.width / display_bounds.size.width;
        }
    }

    /// Updates the internal `frame` of the window by querying its current position and size from the Accessibility API.
    /// It also updates the `width_ratio`.
    ///
    /// # Arguments
    ///
    /// * `display_bounds` - An optional `CGRect` representing the bounds of the display the window is on.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the frame is updated successfully, otherwise `Err(Error)`.
    pub fn update_frame(&mut self, display_bounds: Option<&CGRect>) -> Result<()> {
        // CGRect frame = {0};
        // CFTypeRef position_ref = NULL;
        // CFTypeRef size_ref = NULL;
        //
        // AXUIElementCopyAttributeValue(window->ref, kAXPositionAttribute, &position_ref);
        // AXUIElementCopyAttributeValue(window->ref, kAXSizeAttribute, &size_ref);
        //
        // if (position_ref) {
        //     AXValueGetValue(position_ref, kAXValueTypeCGPoint, &frame.origin);
        //     CFRelease(position_ref);
        // }
        //
        // if (size_ref) {
        //     AXValueGetValue(size_ref, kAXValueTypeCGSize, &frame.size);
        //     CFRelease(size_ref);
        // }
        let window_ref = self.ax_element.as_ptr();

        let position = unsafe {
            let mut position_ref: *mut CFType = null_mut();
            AXUIElementCopyAttributeValue(
                window_ref,
                CFString::from_static_str(kAXPositionAttribute).as_ref(),
                &mut position_ref,
            );
            AXUIWrapper::retain(position_ref)?
        };
        let size = unsafe {
            let mut size_ref: *mut CFType = null_mut();
            AXUIElementCopyAttributeValue(
                window_ref,
                CFString::from_static_str(kAXSizeAttribute).as_ref(),
                &mut size_ref,
            );
            AXUIWrapper::retain(size_ref)?
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
        if !CGRectEqualToRect(frame, self.frame) {
            self.frame = frame;
            self.width_ratio = if let Some(display_bounds) = display_bounds {
                frame.size.width / display_bounds.size.width
            } else {
                0.5
            };
        }
        Ok(())
    }

    /// Makes the window the key window for its application by sending synthesized events.
    ///
    /// # Arguments
    ///
    /// * `psn` - The process serial number of the application.
    fn make_key_window(&self, psn: &ProcessSerialNumber) {
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
        event_bytes[0x3c..0x40].copy_from_slice(&window_id.to_ne_bytes());
        event_bytes[0x20..0x30].fill(0xff);

        event_bytes[0x08] = 0x01;
        unsafe { SLPSPostEventRecordTo(psn, event_bytes.as_ptr().cast()) };

        event_bytes[0x08] = 0x02;
        unsafe { SLPSPostEventRecordTo(psn, event_bytes.as_ptr().cast()) };
    }

    // const CPS_ALL_WINDOWS: u32 = 0x100;
    const CPS_USER_GENERATED: u32 = 0x200;
    // const CPS_NO_WINDOWS: u32 = 0x400;

    /// Focuses the window without raising it. This involves sending specific events to the process.
    ///
    /// # Arguments
    ///
    /// * `currently_focused` - A reference to the currently focused window.
    pub fn focus_without_raise(&self, currently_focused: &Window) {
        let Some((psn, focused_psn)) = self.psn().zip(currently_focused.psn()) else {
            return;
        };
        let window_id = self.id();
        debug!("{}: {window_id}", function_name!());
        if focused_psn == psn {
            let mut event_bytes = [0u8; 0xf8];
            event_bytes[0x04] = 0xf8;
            event_bytes[0x08] = 0x0d;

            event_bytes[0x8a] = 0x02;
            event_bytes[0x3c..0x40].copy_from_slice(&currently_focused.id().to_ne_bytes());
            unsafe {
                SLPSPostEventRecordTo(&focused_psn, event_bytes.as_ptr().cast());
            }
            // @hack
            // Artificially delay the activation by 1ms. This is necessary because some
            // applications appear to be confused if both of the events appear instantaneously.
            thread::sleep(Duration::from_millis(20));

            event_bytes[0x8a] = 0x01;
            event_bytes[0x3c..0x40].copy_from_slice(&window_id.to_ne_bytes());
            unsafe {
                SLPSPostEventRecordTo(&psn, event_bytes.as_ptr().cast());
            }
        }

        unsafe {
            _SLPSSetFrontProcessWithOptions(&psn, window_id, Self::CPS_USER_GENERATED);
        }
        self.make_key_window(&psn);
    }

    /// Focuses the window and raises it to the front.
    pub fn focus_with_raise(&self) {
        let Some(psn) = self.psn else {
            return;
        };
        let window_id = self.id();
        unsafe {
            _SLPSSetFrontProcessWithOptions(&psn, window_id, Self::CPS_USER_GENERATED);
        }
        self.make_key_window(&psn);
        let element_ref = self.ax_element.as_ptr();
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
    fn display_uuid(&self, cid: ConnID) -> Result<CFRetained<CFString>> {
        let window_id = self.id();
        let uuid = unsafe {
            NonNull::new(SLSCopyManagedDisplayForWindow(cid, window_id).cast_mut())
                .map(|uuid| CFRetained::from_raw(uuid))
        };
        uuid.or_else(|| {
            let mut frame = CGRect::default();
            unsafe {
                SLSGetWindowBounds(cid, window_id, &mut frame);
                NonNull::new(SLSCopyBestManagedDisplayForRect(cid, frame).cast_mut())
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
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    ///
    /// # Returns
    ///
    /// `Ok(u32)` with the display ID if successful, otherwise `Err(Error)`.
    fn display_id(&self, cid: ConnID) -> Result<u32> {
        let uuid = self.display_uuid(cid);
        uuid.and_then(|uuid| Display::id_from_uuid(&uuid))
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
        self.frame.origin.x > 0.0
            && self.frame.origin.x < display_bounds.size.width - self.frame.size.width
    }

    /// Centers the mouse cursor on the window if it's not already within the window's bounds.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    pub fn center_mouse(&self, cid: ConnID, display_bounds: &CGRect) {
        // TODO: check for MouseFollowsFocus setting in WindowManager and also whether it's
        // overriden for individual window.

        let mut cursor = CGPoint::default();
        if unsafe { CGError::Success != SLSGetCurrentCursorLocation(cid, &mut cursor) } {
            warn!(
                "{}: Unable to get current cursor position.",
                function_name!()
            );
            return;
        }
        if CGRectContainsPoint(self.frame, cursor) {
            return;
        }

        let center = CGPoint::new(
            display_bounds.origin.x + self.frame.origin.x + self.frame.size.width / 2.0,
            display_bounds.origin.y + self.frame.origin.y + self.frame.size.height / 2.0,
        );
        let display_id = self.display_id(cid);
        #[allow(clippy::redundant_closure)]
        let bounds = display_id.map(|display_id| CGDisplayBounds(display_id));
        if bounds.is_ok_and(|bounds| !CGRectContainsPoint(bounds, center)) {
            return;
        }
        CGWarpMouseCursorPosition(center);
    }

    /// Adjusts the window's position to ensure it is fully exposed (visible on screen) within the display bounds.
    ///
    /// # Arguments
    ///
    /// * `display_bounds` - The `CGRect` representing the bounds of the display.
    /// * `moving` - An optional `RepositionMarker` indicating if the window is currently moving.
    /// * `entity` - The `Entity` of the window.
    /// * `commands` - Bevy commands to trigger events.
    ///
    /// # Returns
    ///
    /// The adjusted `CGRect` of the window after exposure.
    pub fn expose_window(
        &self,
        display_bounds: &CGRect,
        moving: Option<&RepositionMarker>,
        entity: Entity,
        commands: &mut Commands,
    ) -> CGRect {
        // Check if window needs to be fully exposed
        let window_id = self.id();
        let mut frame = if let Some(RepositionMarker { origin }) = moving {
            let mut frame = self.frame;
            frame.origin = *origin;
            frame
        } else {
            self.frame
        };
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
            commands.entity(entity).insert(RepositionMarker {
                origin: frame.origin,
            });
            trace!("{}: focus resposition to {frame:?}", function_name!());
        }
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::prelude::*;

    fn setup_world_and_pane() -> (World, WindowPane, Vec<Entity>) {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut pane = WindowPane::default();
        pane.append(entities[0]);
        pane.append(entities[1]);
        pane.append(entities[2]);

        (world, pane, entities)
    }

    #[test]
    fn test_window_pane_index_of() {
        let (_world, pane, entities) = setup_world_and_pane();
        assert_eq!(pane.index_of(entities[0]).unwrap(), 0);
        assert_eq!(pane.index_of(entities[1]).unwrap(), 1);
        assert_eq!(pane.index_of(entities[2]).unwrap(), 2);
    }

    #[test]
    fn test_window_pane_swap() {
        let (_world, mut pane, entities) = setup_world_and_pane();
        pane.swap(0, 2);
        assert_eq!(pane.index_of(entities[2]).unwrap(), 0);
        assert_eq!(pane.index_of(entities[0]).unwrap(), 2);
    }

    #[test]
    fn test_window_pane_stack_and_unstack() {
        let (_world, mut pane, entities) = setup_world_and_pane();

        // Stack [1] onto [0]
        pane.stack(entities[1]).unwrap();
        assert_eq!(pane.len(), 2);
        assert_eq!(pane.index_of(entities[0]).unwrap(), 0);
        assert_eq!(pane.index_of(entities[1]).unwrap(), 0); // Both in the same panel

        // Check internal structure
        match pane.get(0).unwrap() {
            Panel::Stack(stack) => {
                assert_eq!(stack.len(), 2);
                assert_eq!(stack[0], entities[0]);
                assert_eq!(stack[1], entities[1]);
            }
            Panel::Single(_) => panic!("Expected a stack"),
        }

        // Unstack [0]
        pane.unstack(entities[0]).unwrap();
        assert_eq!(pane.len(), 3);
        assert_eq!(pane.index_of(entities[1]).unwrap(), 0);
        assert_eq!(pane.index_of(entities[0]).unwrap(), 1);
        assert_eq!(pane.index_of(entities[2]).unwrap(), 2);
    }
}

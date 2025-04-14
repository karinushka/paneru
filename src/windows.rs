use accessibility_sys::{
    AXObserverRef, AXUIElementRef, AXValueCreate, AXValueGetValue, kAXErrorSuccess,
    kAXFloatingWindowSubrole, kAXMinimizedAttribute, kAXParentAttribute, kAXPositionAttribute,
    kAXRaiseAction, kAXRoleAttribute, kAXSizeAttribute, kAXStandardWindowSubrole,
    kAXSubroleAttribute, kAXTitleAttribute, kAXUnknownSubrole, kAXValueTypeCGPoint,
    kAXValueTypeCGSize, kAXWindowRole,
};
use core::ptr::NonNull;
use log::{debug, error, info, trace, warn};
use objc2::rc::{Retained, autoreleasepool};
use objc2_core_foundation::{
    CFArray, CFArrayGetCount, CFBoolean, CFBooleanGetValue, CFDataCreateMutable,
    CFDataGetMutableBytePtr, CFDataIncreaseLength, CFEqual, CFNumber, CFNumberGetType,
    CFNumberGetValue, CFNumberType, CFRetained, CFString, CFType, CFUUIDCreateFromString,
    CFUUIDCreateString, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGError, CGGetActiveDisplayList, CGRectContainsPoint,
    CGRectEqualToRect, CGWarpMouseCursorPosition,
};
use std::collections::HashMap;
use std::io::{Error, ErrorKind, Result};
use std::ops::Deref;
use std::ptr::null_mut;
use std::slice::from_raw_parts_mut;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;
use stdext::function_name;
use stdext::prelude::RwLockExt;

use crate::app::Application;
use crate::events::EventSender;
use crate::platform::{
    AXObserverAddNotification, AXObserverRemoveNotification, Pid, ProcessInfo, ProcessSerialNumber,
    get_process_info,
};
use crate::process::{Process, ProcessManager};
use crate::skylight::{
    _AXUIElementCreateWithRemoteToken, _AXUIElementGetWindow, _SLPSGetFrontProcess,
    _SLPSSetFrontProcessWithOptions, AXUIElementCopyAttributeValue, AXUIElementPerformAction,
    AXUIElementSetAttributeValue, CGDisplayCreateUUIDFromDisplayID, CGDisplayGetDisplayIDFromUUID,
    ConnID, SLPSPostEventRecordTo, SLSCopyActiveMenuBarDisplayIdentifier,
    SLSCopyBestManagedDisplayForRect, SLSCopyManagedDisplayForWindow, SLSCopyManagedDisplaySpaces,
    SLSCopyWindowsWithOptionsAndTags, SLSGetCurrentCursorLocation, SLSGetWindowBounds,
    SLSManagedDisplayGetCurrentSpace, SLSWindowIteratorAdvance, SLSWindowIteratorGetAttributes,
    SLSWindowIteratorGetCount, SLSWindowIteratorGetParentID, SLSWindowIteratorGetTags,
    SLSWindowIteratorGetWindowID, SLSWindowQueryResultCopyWindows, SLSWindowQueryWindows, WinID,
};
use crate::util::{
    AxuWrapperType, create_array, get_array_values, get_attribute, get_cfdict_value,
};

const THRESHOLD: f64 = 10.0;

#[derive(Clone, Default)]
pub struct WindowPane {
    pane: Arc<RwLock<Vec<WinID>>>,
}

impl WindowPane {
    pub fn index_of(&self, window_id: WinID) -> Result<usize> {
        self.pane
            .force_read()
            .iter()
            .position(|id| *id == window_id)
            .ok_or(Error::new(
                ErrorKind::NotFound,
                format!(
                    "{}: can not find window {window_id} in the current pane.",
                    function_name!()
                ),
            ))
    }

    pub fn insert_at(&self, after: usize, window_id: WinID) -> Result<usize> {
        let index = after + 1;
        if index > self.len() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("{}: index {after} out of bounds.", function_name!()),
            ));
        }
        self.pane.force_write().insert(index, window_id);
        Ok(index)
    }

    pub fn append(&self, window_id: WinID) {
        self.pane.force_write().push(window_id);
    }

    pub fn remove(&self, window_id: WinID) {
        self.pane.force_write().retain(|id| *id != window_id);
    }

    pub fn get(&self, at: usize) -> Result<WinID> {
        self.pane.force_read().get(at).cloned().ok_or(Error::new(
            ErrorKind::InvalidInput,
            format!("{}: {at} out of bounds", function_name!()),
        ))
    }

    pub fn swap(&self, left: usize, right: usize) {
        self.pane.force_write().swap(left, right);
    }

    pub fn len(&self) -> usize {
        self.pane.force_read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.pane.force_read().is_empty()
    }

    pub fn first(&self) -> Result<WinID> {
        self.pane.force_read().first().cloned().ok_or(Error::new(
            ErrorKind::NotFound,
            format!("{}: can not find first element.", function_name!()),
        ))
    }

    pub fn last(&self) -> Result<WinID> {
        self.pane.force_read().last().cloned().ok_or(Error::new(
            ErrorKind::NotFound,
            format!("{}: can not find last element.", function_name!()),
        ))
    }

    pub fn access_right_of(
        &self,
        window_id: WinID,
        mut accessor: impl FnMut(WinID) -> bool,
    ) -> Result<()> {
        let index = self.index_of(window_id)?;
        for id in self.pane.force_read()[1 + index..].iter() {
            if !accessor(*id) {
                break;
            }
        }
        Ok(())
    }

    pub fn access_left_of(
        &self,
        window_id: WinID,
        mut accessor: impl FnMut(WinID) -> bool,
    ) -> Result<()> {
        let index = self.index_of(window_id)?;
        for id in self.pane.force_read()[0..index].iter().rev() {
            // NOTE: left side iterates backwards.
            if !accessor(*id) {
                break;
            }
        }
        Ok(())
    }
}

pub struct Display {
    id: CGDirectDisplayID,
    // Map of workspaces, containing panels of windows.
    spaces: HashMap<u64, WindowPane>,
    bounds: CGRect,
}

impl Display {
    fn new(id: CGDirectDisplayID, spaces: Vec<u64>) -> Self {
        let spaces = HashMap::from_iter(spaces.into_iter().map(|id| (id, WindowPane::default())));
        let bounds = unsafe { CGDisplayBounds(id) };
        Self { id, spaces, bounds }
    }

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

    fn id_from_uuid(uuid: CFRetained<CFString>) -> Result<u32> {
        unsafe {
            let id = CFUUIDCreateFromString(None, Some(&uuid)).ok_or(Error::new(
                ErrorKind::NotFound,
                format!("{}: can not convert from {uuid}.", function_name!()),
            ))?;
            Ok(CGDisplayGetDisplayIDFromUUID(id.deref()))
        }
    }

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

    fn present_displays(cid: ConnID) -> Vec<Self> {
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

    fn active_display_id(cid: ConnID) -> Result<u32> {
        let uuid = Display::active_display_uuid(cid)?;
        Display::id_from_uuid(uuid)
    }

    fn active_display_space(&self, cid: ConnID) -> Result<u64> {
        Display::uuid_from_id(self.id)
            .map(|uuid| unsafe { SLSManagedDisplayGetCurrentSpace(cid, uuid.deref()) })
    }

    pub fn active_panel(&self, cid: ConnID) -> Result<WindowPane> {
        let space_id = self.active_display_space(cid)?;
        self.spaces.get(&space_id).cloned().ok_or(Error::new(
            ErrorKind::NotFound,
            format!("{}: can not find space {space_id}.", function_name!()),
        ))
    }

    pub fn remove_window(&self, window_id: WinID) {
        self.spaces.values().for_each(|pane| pane.remove(window_id));
    }
}

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

fn ax_window_pid(element_ref: &CFRetained<AxuWrapperType>) -> Result<Pid> {
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

#[derive(Debug, Clone)]
pub struct Window {
    inner: Arc<RwLock<InnerWindow>>,
}

#[derive(Debug)]
pub struct InnerWindow {
    pub id: WinID,
    pub app: Application,
    element_ref: CFRetained<AxuWrapperType>,
    frame: CGRect,
    minimized: bool,
    is_root: bool,
    observing: Vec<bool>,
    size_ratios: Vec<f64>,
    width_ratio: f64,
    managed: bool,
}

impl Window {
    pub fn id(&self) -> WinID {
        self.inner().id
    }

    pub fn app(&self) -> Application {
        self.inner().app.clone()
    }

    pub fn frame(&self) -> CGRect {
        self.inner().frame
    }

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

    pub fn managed(&self) -> bool {
        self.inner().managed
    }

    pub fn manage(&self, manage: bool) {
        self.inner.force_write().managed = manage;
    }

    fn inner(&self) -> std::sync::RwLockReadGuard<'_, InnerWindow> {
        self.inner.force_read()
    }

    fn parent(main_conn: ConnID, window_id: WinID) -> Result<WinID> {
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

    fn title(&self) -> Result<String> {
        let axtitle = CFString::from_static_str(kAXTitleAttribute);
        let title = get_attribute::<CFString>(&self.inner().element_ref, axtitle)?;
        Ok(title.to_string())
    }

    pub fn role(&self) -> Result<String> {
        let axrole = CFString::from_static_str(kAXRoleAttribute);
        let role = get_attribute::<CFString>(&self.inner().element_ref, axrole)?;
        Ok(role.to_string())
    }

    fn subrole(&self) -> Result<String> {
        let axrole = CFString::from_static_str(kAXSubroleAttribute);
        let role = get_attribute::<CFString>(&self.inner().element_ref, axrole)?;
        Ok(role.to_string())
    }

    fn is_unknown(&self) -> bool {
        self.subrole()
            .is_ok_and(|subrole| subrole.eq(kAXUnknownSubrole))
    }

    fn is_minimized(&self) -> bool {
        let axminimized = CFString::from_static_str(kAXMinimizedAttribute);
        get_attribute::<CFBoolean>(&self.inner().element_ref, axminimized)
            .map(|minimized| unsafe { CFBooleanGetValue(minimized.deref()) })
            .is_ok_and(|minimized| minimized || self.inner().minimized)
    }

    fn is_root(&self) -> bool {
        let inner = self.inner();
        let cftype = inner.element_ref.as_ref();
        let axparent = CFString::from_static_str(kAXParentAttribute);
        get_attribute::<CFType>(&self.inner().element_ref, axparent)
            .is_ok_and(|parent| !CFEqual(Some(parent.deref()), Some(cftype)))
    }

    fn is_real(&self) -> bool {
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

    pub fn is_eligible(&self) -> bool {
        let me = self.inner();
        me.is_root && self.is_real() // TODO: check for WINDOW_RULE_MANAGED
    }

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
                    self.inner().element_ref.as_ptr(),
                    CFString::from_static_str(kAXPositionAttribute).as_ref(),
                    position.as_ref(),
                )
            };
            self.inner.force_write().frame.origin = point;
        }
    }

    pub fn resize(&self, width: f64, height: f64, display_bounds: &CGRect) {
        let mut size = CGSize::new(width, height);
        let size_ref =
            unsafe { AXValueCreate(kAXValueTypeCGSize, NonNull::from(&mut size).as_ptr().cast()) };
        if let Ok(position) = AxuWrapperType::retain(size_ref) {
            unsafe {
                AXUIElementSetAttributeValue(
                    self.inner().element_ref.as_ptr(),
                    CFString::from_static_str(kAXSizeAttribute).as_ref(),
                    position.as_ref(),
                )
            };
            let mut inner = self.inner.force_write();
            inner.frame.size = size;
            inner.width_ratio = size.width / display_bounds.size.width;
        }
    }

    fn update_frame(&self, display_bounds: &CGRect) -> Result<()> {
        let window_ref = self.inner().element_ref.as_ptr();

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

    fn make_key_window(&self) {
        let psn = self.app().psn();
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

    pub fn focus_without_raise(&self, window_manager: &WindowManager) {
        let psn = self.app().psn();
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

    pub fn focus_with_raise(&self) {
        let psn = self.app().psn();
        let window_id = self.id();
        unsafe {
            _SLPSSetFrontProcessWithOptions(&psn, window_id, Self::CPS_USER_GENERATED);
        }
        self.make_key_window();
        let element_ref = self.inner().element_ref.as_ptr();
        let action = CFString::from_static_str(kAXRaiseAction);
        unsafe { AXUIElementPerformAction(element_ref, &action) };
    }

    fn observe(&self) -> bool {
        let observer_ref = match self.app().observer_ref() {
            None => {
                warn!(
                    "{}: can not observe window {} without application observer.",
                    function_name!(),
                    self.id(),
                );
                return false;
            }
            Some(observer_ref) => observer_ref,
        };
        let observing = crate::app::AX_WINDOW_NOTIFICATIONS
            .iter()
            .map(|name| unsafe {
                let notification = CFString::from_static_str(name);
                debug!(
                    "{}: {name} {:?} {:?}",
                    function_name!(),
                    observer_ref.as_ptr::<AXObserverRef>(),
                    self.inner().element_ref.as_ptr::<AXUIElementRef>(),
                );
                match AXObserverAddNotification(
                    observer_ref.as_ptr(),
                    self.inner().element_ref.as_ptr(),
                    notification.deref(),
                    NonNull::from(self.inner.deref()).as_ptr().cast(),
                ) {
                    accessibility_sys::kAXErrorSuccess
                    | accessibility_sys::kAXErrorNotificationAlreadyRegistered => true,
                    result => {
                        error!(
                            "{}: error registering {name} for window {}: {result}",
                            function_name!(),
                            self.id()
                        );
                        false
                    }
                }
            })
            .collect::<Vec<_>>();
        let gotall = observing.iter().all(|status| *status);

        self.inner.force_write().observing = observing;
        gotall
    }

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

    fn display_id(&self, cid: ConnID) -> Result<u32> {
        let uuid = self.display_uuid(cid);
        uuid.and_then(|uuid| Display::id_from_uuid(uuid.into()))
    }

    pub fn fully_visible(&self, display_bounds: &CGRect) -> bool {
        let frame = self.inner().frame;
        frame.origin.x > 0.0 && frame.origin.x < display_bounds.size.width - frame.size.width
    }

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

    // Fully expose the window if parts of it are off-screen.
    fn expose_window(&self, display_bounds: &CGRect) -> CGRect {
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

impl InnerWindow {
    fn unobserve(&mut self) {
        let observer: AXObserverRef = match self.app.observer_ref() {
            Some(observer) => observer.as_ptr(),
            None => {
                error!(
                    "{}: No application reference to unregister a window {}",
                    function_name!(),
                    self.id
                );
                return;
            }
        };
        crate::app::AX_WINDOW_NOTIFICATIONS
            .iter()
            .zip(&self.observing)
            .filter(|(_, remove)| **remove)
            .for_each(|(name, _)| {
                let notification = CFString::from_static_str(name);
                debug!(
                    "{}: {name} {:?} {:?}",
                    function_name!(),
                    observer,
                    self.element_ref.as_ptr::<AXUIElementRef>(),
                );
                let result = unsafe {
                    AXObserverRemoveNotification(
                        observer,
                        self.element_ref.as_ptr(),
                        notification.deref(),
                    )
                };
                if result != kAXErrorSuccess {
                    warn!(
                        "{}: error unregistering {name} for window {}: {result}",
                        function_name!(),
                        self.id
                    );
                }
            });
    }
}

impl Drop for InnerWindow {
    fn drop(&mut self) {
        self.unobserve();
    }
}

pub struct WindowManager {
    pub events: EventSender,
    pub applications: HashMap<Pid, Application>,
    pub main_cid: ConnID,
    last_window: Option<WinID>, // TODO: use this for "goto last window bind"
    pub focused_window: Option<WinID>,
    focused_psn: ProcessSerialNumber,
    pub ffm_window_id: Option<WinID>,
    pub mission_control_is_active: bool,
    pub skip_reshuffle: bool,
    pub mouse_down_window: Option<Window>,
    pub down_location: CGPoint,
    displays: Vec<Display>,
    pub focus_follows_mouse: bool,
}

impl WindowManager {
    pub fn new(events: EventSender, main_cid: ConnID) -> Result<Self> {
        let displays = Display::present_displays(main_cid);
        if displays.is_empty() {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("{}: Can not find any displays?!", function_name!()),
            ));
        }

        Ok(WindowManager {
            events,
            applications: HashMap::new(),
            main_cid,
            last_window: None,
            focused_window: None,
            focused_psn: ProcessSerialNumber::default(),
            ffm_window_id: None,
            mission_control_is_active: false,
            skip_reshuffle: false,
            mouse_down_window: None,
            down_location: CGPoint::default(),
            displays,
            focus_follows_mouse: true,
        })
    }

    pub fn start(&mut self, process_manager: &mut ProcessManager) {
        autoreleasepool(|_| {
            for process in process_manager.processes.values_mut() {
                if process.is_observable() {
                    let app = match Application::from_process(
                        self.main_cid,
                        process,
                        self.events.clone(),
                    ) {
                        Ok(app) => app,
                        Err(err) => {
                            error!("{}: error creating applicatoin: {err}", function_name!());
                            return;
                        }
                    };
                    debug!(
                        "{}: Application {} is observable",
                        function_name!(),
                        app.name()
                    );

                    if app.observe().is_ok_and(|result| result) {
                        self.applications.insert(app.pid(), app.clone());
                        _ = self
                            .add_existing_application_windows(&app, 0)
                            .inspect_err(|err| warn!("{}: {err}", function_name!()));
                    } else {
                        // app.unobserve() handled by the Drop.
                    }
                } else {
                    // println!(
                    //     "{} ({}) is not observable, subscribing to activationPolicy changes",
                    //     process.name, process.pid
                    // );
                    // workspace_application_observe_activation_policy(g_workspace_context, process);
                }
            }
        });

        for display in self.displays.iter() {
            for (space_id, pane) in display.spaces.iter() {
                self.refresh_windows_space(*space_id, pane);
            }
        }

        if let Ok(window) = self.focused_window() {
            self.last_window = Some(window.id());
            self.focused_window = Some(window.id());
            self.focused_psn = window.app().psn();
        }
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

    pub fn find_application(&self, pid: Pid) -> Option<Application> {
        self.applications.get(&pid).cloned()
    }

    pub fn find_window(&self, window_id: WinID) -> Option<Window> {
        self.applications
            .values()
            .find_map(|app| app.find_window(window_id))
    }

    fn space_window_list_for_connection(
        &self,
        spaces: Vec<u64>,
        cid: Option<ConnID>,
        also_minimized: bool,
    ) -> Result<Vec<WinID>> {
        unsafe {
            let space_list_ref = create_array(spaces, CFNumberType::SInt64Type)?;

            let set_tags = 0i64;
            let clear_tags = 0i64;
            let options = if also_minimized { 0x7 } else { 0x2 };
            let ptr = NonNull::new(SLSCopyWindowsWithOptionsAndTags(
                self.main_cid,
                cid.unwrap_or(0),
                space_list_ref.deref(),
                options,
                &set_tags,
                &clear_tags,
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

                match self.find_window(wid) {
                    Some(window) => {
                        if also_minimized || !window.is_minimized() {
                            window_list.push(window.id());
                        }
                    }
                    None => {
                        if parent_wid == 0
                            && ((0 != (attributes & 0x2) || 0 != (tags & 0x400000000000000))
                                && (0 != (tags & 0x1)
                                    || (0 != (tags & 0x2) && 0 != (tags & 0x80000000))))
                            || ((attributes == 0x0 || attributes == 0x1)
                                && (0 != (tags & 0x1000000000000000)
                                    || 0 != (tags & 0x300000000000000))
                                && (0 != (tags & 0x1)
                                    || (0 != (tags & 0x2) && 0 != (tags & 0x80000000))))
                        {
                            window_list.push(wid);
                        }
                    }
                }
            }
            Ok(window_list)
        }
    }

    fn existing_application_window_list(&self, app: &Application) -> Result<Vec<WinID>> {
        let spaces: Vec<u64> = self
            .displays
            .iter()
            .flat_map(|display| display.spaces.keys().cloned().collect::<Vec<_>>())
            .collect();
        debug!("{}: spaces {spaces:?}", function_name!());
        if spaces.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("{}: no spaces returned", function_name!()),
            ));
        }
        self.space_window_list_for_connection(spaces, app.connection(), true)
    }

    fn bruteforce_windows(&mut self, app: &Application, window_list: &mut Vec<WinID>) {
        debug!(
            "{}: App {} has unresolved window on other desktops, bruteforcing them.",
            function_name!(),
            app.name()
        );
        //
        // NOTE(koekeishiya): MacOS API does not return AXUIElementRef of windows on inactive spaces.
        // However, we can just brute-force the element_id and create the AXUIElementRef ourselves.
        //
        // :Attribution
        // https://github.com/decodism
        // https://github.com/lwouis/alt-tab-macos/issues/1324#issuecomment-2631035482
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

            // uint8_t *data = CFDataGetMutableBytePtr(data_ref);
            // *(uint32_t *) (data + 0x0) = application->pid;
            // *(uint32_t *) (data + 0x8) = 0x636f636f;
            const MAGIC: u32 = 0x636f636f;
            let data = from_raw_parts_mut(
                CFDataGetMutableBytePtr(data_ref.deref().into()),
                BUFSIZE as usize,
            );
            let bytes = app.pid().to_ne_bytes();
            data[0x0..bytes.len()].copy_from_slice(&bytes);
            let bytes = MAGIC.to_ne_bytes();
            data[0x8..0x8 + bytes.len()].copy_from_slice(&bytes);

            // for (uint64_t element_id = 0; element_id < 0x7fff; ++element_id) {
            for element_id in 0..0x7fffu64 {
                // int app_window_list_len = ts_buf_len(app_window_list);
                // if (app_window_list_len == 0) break;

                //
                // NOTE(koekeishiya): Only the element_id changes between iterations.
                //

                //     memcpy(data+0xc, &element_id, sizeof(uint64_t));
                //     AXUIElementRef element_ref = _AXUIElementCreateWithRemoteToken(data_ref);
                //     uint32_t element_wid = ax_window_id(element_ref);
                //     bool matched = false;
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

                //     if (element_wid != 0) {
                //         for (int i = 0; i < app_window_list_len; ++i) {
                //             if (app_window_list[i] == element_wid) {
                //                 matched = true;
                //                 ts_buf_del(app_window_list, i);
                //                 break;
                //             }
                //         }
                //     }
                //     if (matched) {
                //         window_manager_create_and_add_window(sm, wm, application, element_ref, element_wid, false);
                //     } else {
                //         CFRelease(element_ref);
                //     }

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

        let global_window_list = self.existing_application_window_list(app)?;
        if global_window_list.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "{}: No windows found for app {}",
                    function_name!(),
                    app.name(),
                ),
            ));
        }
        info!(
            "{}: App {} has global windows: {global_window_list:?}",
            function_name!(),
            app.name()
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

                // FIXME: The AX API appears to always include a single element for Finder that
                // returns an empty window id. This is likely the desktop window. Other similar
                // cases should be handled the same way; simply ignore the window when we attempt
                // to do an equality check to see if we have correctly discovered the number of
                // windows to track.

                if self.find_window(window_id).is_none() {
                    let window_ref = AxuWrapperType::retain(window_ref.as_ptr())?;
                    info!(
                        "{}: Add window: {} {window_id}",
                        function_name!(),
                        app.name()
                    );
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
                    app.name()
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
                    app.name()
                );
                self.bruteforce_windows(app, &mut app_window_list);
            }
        }

        Ok(result)
    }

    pub fn add_application_windows(&mut self, app: &Application) -> Result<Vec<Window>> {
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

    fn new_window(
        &self,
        app: &Application,
        element_ref: CFRetained<AxuWrapperType>,
        window_id: WinID,
    ) -> Result<Window> {
        let window = Window {
            inner: Arc::new(RwLock::new(InnerWindow {
                id: window_id,
                app: app.clone(),
                element_ref,
                frame: CGRect::default(),
                minimized: false,
                is_root: false,
                observing: vec![],
                size_ratios: vec![0.25, 0.33, 0.50, 0.66, 0.75],
                width_ratio: 0.33,
                managed: true,
            })),
        };
        window.update_frame(&self.current_display_bounds()?)?;

        let connection = app.connection().ok_or(Error::new(
            ErrorKind::InvalidData,
            format!("{}: invalid connection for window.", function_name!()),
        ))?;
        let minimized = window.is_minimized();
        // window->is_root = !window_parent(window->id) || window_is_root(window);
        let is_root = Window::parent(connection, window_id).is_err() || window.is_root();
        {
            let mut inner = window.inner.force_write();
            inner.minimized = minimized;
            inner.is_root = is_root;
        }
        Ok(window)
    }
    fn create_and_add_window(
        &mut self,
        app: &Application,
        window_ref: CFRetained<AxuWrapperType>,
        window_id: WinID,
        _one_shot_rules: bool, // TODO: fix
    ) -> Result<Window> {
        let window = self.new_window(app, window_ref, window_id)?;
        if window.is_unknown() {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "{}: Ignoring AXUnknown window, app: {} id: {}",
                    function_name!(),
                    app.name(),
                    window.id()
                ),
            ));
        }

        if !window.is_real() {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "{}: Ignoring non-real window, app: {} id: {}",
                    function_name!(),
                    app.name(),
                    window.id()
                ),
            ));
        }

        info!(
            "{}: created {} app: {} title: {} role: {} subrole: {}",
            function_name!(),
            window.id(),
            app.name(),
            window.title().unwrap_or_default(),
            window.role().unwrap_or_default(),
            window.subrole().unwrap_or_default(),
        );

        //
        // NOTE(koekeishiya): Attempt to track **all** windows.
        //

        if !window.observe() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!(
                    "{}: Could not observe window {} of {}",
                    function_name!(),
                    window.id(),
                    app.name()
                ),
            ));
        }

        app.add_window(&window);
        Ok(window)
    }

    fn focused_application(&self) -> Result<Application> {
        let mut psn = ProcessSerialNumber::default();
        let mut pinfo = ProcessInfo::default();
        unsafe {
            _SLPSGetFrontProcess(&mut psn);
            get_process_info(&psn, &mut pinfo);
        }
        self.find_application(pinfo.pid).ok_or(Error::new(
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

    pub fn front_switched(&mut self, process: &mut Process) {
        let app = match self.find_application(process.pid) {
            Some(app) => app,
            None => {
                warn!(
                    "{}: window_manager_add_lost_front_switched_event",
                    function_name!()
                );
                return;
            }
        };
        debug!("{}: {}", function_name!(), app.name());

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
                self.focused_psn = app.psn();
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
    }

    pub fn window_created(&mut self, element_ref: CFRetained<AxuWrapperType>) -> Result<()> {
        let window_id = ax_window_id(element_ref.as_ptr())?;
        if self.find_window(window_id).is_some() {
            return Err(Error::new(
                ErrorKind::AlreadyExists,
                format!("{}: window {window_id} already created.", function_name!()),
            ));
        }

        let pid = ax_window_pid(&element_ref)?;
        let app = self.find_application(pid).ok_or(Error::new(
            ErrorKind::NotFound,
            format!(
                "{}: unable to find application with {pid}.",
                function_name!()
            ),
        ))?;

        let window = self.create_and_add_window(&app, element_ref, window_id, true)?;
        info!(
            "{}: created {} app: {} title: {} role: {} subrole: {}",
            function_name!(),
            window.id(),
            app.name(),
            window.title().unwrap_or_default(),
            window.role().unwrap_or_default(),
            window.subrole().unwrap_or_default(),
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

    pub fn window_destroyed(&mut self, window_id: WinID) {
        self.displays.iter().for_each(|display| {
            display.remove_window(window_id);
        });

        let app = self.find_window(window_id).map(|window| window.app());
        if let Some(window) = app.and_then(|app| app.remove_window(window_id)) {
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

    pub fn window_moved(&self, _window_id: WinID) {
        // uint32_t window_id = (uint32_t)(intptr_t) context;
    }

    pub fn window_resized(&self, window_id: WinID) -> Result<()> {
        if let Some(window) = self.find_window(window_id) {
            window.update_frame(&self.current_display_bounds()?)?;
            self.reshuffle_around(&window)?;
        }
        Ok(())
    }

    pub fn window_focused(&mut self, window: Window) {
        let focused_id = self.focused_window;
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
        self.focused_psn = window.app().psn();
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

    // Searches other inactive displays for windows which are currently on this display and
    // relocates them.
    pub fn add_detected_display(&mut self) -> Result<&Display> {
        let id = Display::active_display_id(self.main_cid)?;
        let uuid = Display::uuid_from_id(id)?;
        info!("{}: detected new display {id} ({uuid}).", function_name!());
        let spaces = Display::display_space_list(uuid.deref(), self.main_cid)?;
        let display = Display::new(id, spaces);
        let display_bounds = self.current_display_bounds()?;

        for (space_id, pane) in display.spaces.iter() {
            // Populate the display panes with its windows.
            self.refresh_windows_space(*space_id, pane);
            pane.access_right_of(pane.first()?, |window_id| {
                // Remove this window from any other displays.
                self.displays
                    .iter()
                    .for_each(|display| display.remove_window(window_id));
                debug!(
                    "{}: Moved window {} to new display.",
                    function_name!(),
                    window_id
                );
                if let Some(window) = self.find_window(window_id) {
                    window.resize(
                        display_bounds.size.width * window.inner().width_ratio,
                        display_bounds.size.height,
                        &display_bounds,
                    );
                    _ = window.update_frame(&display_bounds);
                }
                true // continue through all windows.
            })?;
        }

        // Remove displays without any active windows.
        self.displays
            .retain(|display| display.spaces.values().any(|pane| !pane.is_empty()));

        self.displays.push(display);
        self.displays.last().ok_or(Error::new(
            ErrorKind::NotFound,
            format!("{}: could not find a display", function_name!()),
        ))
    }

    pub fn delete_application(&mut self, pid: Pid) {
        if let Some(app) = self.applications.remove(&pid) {
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
}

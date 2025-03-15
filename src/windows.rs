use accessibility_sys::{
    AXUIElementRef, AXValueCreate, AXValueGetValue, kAXDialogSubrole, kAXFloatingWindowSubrole,
    kAXMinimizedAttribute, kAXParentAttribute, kAXPositionAttribute, kAXRaiseAction,
    kAXRoleAttribute, kAXSizeAttribute, kAXStandardWindowSubrole, kAXSubroleAttribute,
    kAXTitleAttribute, kAXUnknownSubrole, kAXValueTypeCGPoint, kAXValueTypeCGSize, kAXWindowRole,
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
use std::ffi::c_void;
use std::ops::Deref;
use std::ptr::null_mut;
use std::slice::from_raw_parts_mut;
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use crate::app::{Application, ApplicationHandler};
use crate::events::Event;
use crate::platform::{Pid, ProcessInfo, ProcessSerialNumber, get_process_info};
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
    get_string_from_string,
};

const THRESHOLD: f64 = 10.0;
pub const SCREEN_WIDTH: f64 = 3840.0; // TODO: Read screen width somewhere.
pub const SCREEN_HEIGHT: f64 = 2160.0 - THRESHOLD;

type WindowPane = Arc<RwLock<Vec<Window>>>;

pub struct Display {
    id: CGDirectDisplayID,
    // Map of workspaces, containing panels of windows.
    spaces: HashMap<u64, WindowPane>,
}

impl Display {
    fn uuid_from_id(id: CGDirectDisplayID) -> Option<CFRetained<CFString>> {
        unsafe {
            let uuid = CFRetained::from_raw(NonNull::new(CGDisplayCreateUUIDFromDisplayID(id))?);
            CFUUIDCreateString(None, Some(&uuid))
        }
    }

    fn id_from_uuid(uuid: CFRetained<CFString>) -> Option<u32> {
        Some(unsafe {
            let id = CFUUIDCreateFromString(None, Some(&uuid))?;
            CGDisplayGetDisplayIDFromUUID(id.deref())
        })
    }

    fn display_space_list(uuid: &CFString, cid: ConnID) -> Option<Vec<u64>> {
        // let uuid = DisplayManager::display_uuid(display)?;
        unsafe {
            let display_spaces = NonNull::new(SLSCopyManagedDisplaySpaces(cid))
                .map(|ptr| CFRetained::from_raw(ptr))?;

            for display in get_array_values(display_spaces.as_ref()) {
                trace!("display_space_list: display {:?}", display.as_ref());
                let identifier = get_cfdict_value::<CFString>(
                    display.as_ref(),
                    CFString::from_static_str("Display Identifier").deref(),
                )?;
                debug!(
                    "display_space_list: identifier {:?} uuid {:?}",
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
                debug!("display_space_list: spaces {spaces:?}");

                let space_list = get_array_values(spaces.as_ref())
                    .map(|space| {
                        let num = get_cfdict_value::<CFNumber>(
                            space.as_ref(),
                            CFString::from_static_str("id64").deref(),
                        )
                        .unwrap();

                        let id = 0u64;
                        CFNumberGetValue(
                            num.as_ref(),
                            CFNumberGetType(num.as_ref()),
                            (&id as *const u64) as *mut c_void,
                        );
                        id
                    })
                    .collect::<Vec<u64>>();
                return Some(space_list);
            }
        }
        None
    }

    fn active_displays(cid: ConnID) -> Vec<Self> {
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
                        .map(|ids| {
                            ids.into_iter()
                                .map(|id| (id, Arc::new(RwLock::new(vec![]))))
                        })
                        .map(HashMap::from_iter)
                        .map(|spaces| Display { id, spaces })
                })
            })
            .collect()
    }

    fn active_display_uuid(cid: ConnID) -> Option<CFRetained<CFString>> {
        unsafe {
            let ptr = SLSCopyActiveMenuBarDisplayIdentifier(cid);
            Some(CFRetained::from_raw(NonNull::new(ptr as *mut CFString)?))
        }
    }

    fn active_display_id(cid: ConnID) -> Option<u32> {
        let uuid = Display::active_display_uuid(cid)?;
        Display::id_from_uuid(uuid)
    }

    fn active_display_space(&self, cid: ConnID) -> Option<u64> {
        Display::uuid_from_id(self.id)
            .map(|uuid| unsafe { SLSManagedDisplayGetCurrentSpace(cid, uuid.deref()) })
    }
}

pub fn ax_window_id(element_ref: AXUIElementRef) -> Option<WinID> {
    NonNull::new(element_ref).and_then(|ptr| {
        let mut window_id: WinID = 0;
        unsafe { _AXUIElementGetWindow(ptr.as_ptr(), &mut window_id) };
        (window_id != 0).then_some(window_id)
    })
}

fn ax_window_pid(element_ref: &CFRetained<AxuWrapperType>) -> Option<Pid> {
    let pid: Pid = unsafe {
        NonNull::new_unchecked(element_ref.as_ptr::<Pid>())
            .byte_add(0x10)
            .read()
    };
    (pid != 0).then_some(pid)
}

#[derive(Debug, Clone)]
pub struct Window {
    pub inner: Arc<RwLock<InnerWindow>>,
}

#[derive(Debug)]
pub struct InnerWindow {
    pub id: WinID,
    pub app: Application,
    pub element_ref: CFRetained<AxuWrapperType>,
    pub frame: CGRect,
    pub minimized: bool,
    pub is_root: bool,
    pub observing: Vec<bool>,
    pub sizes: Vec<f64>,
}

impl Window {
    fn new(app: &Application, element_ref: CFRetained<AxuWrapperType>, window_id: WinID) -> Self {
        let window = Window {
            inner: Arc::new(RwLock::new(InnerWindow {
                id: window_id,
                app: app.clone(),
                element_ref,
                frame: CGRect::default(),
                minimized: false,
                is_root: false,
                // handler: WindowHandler::new(app),
                observing: vec![],
                sizes: vec![
                    SCREEN_WIDTH * 0.25,
                    SCREEN_WIDTH * 0.33,
                    SCREEN_WIDTH * 0.50,
                    SCREEN_WIDTH * 0.66,
                    SCREEN_WIDTH * 0.75,
                ],
            })),
        };
        window.update_frame();

        let minimized = window.is_minimized();
        // window->is_root = !window_parent(window->id) || window_is_root(window);
        let is_root = Window::parent(
            app.inner().connection.expect("No App connection found."),
            window_id,
        )
        .is_none()
            || window.is_root();
        {
            let mut inner = window.inner.write().unwrap();
            inner.minimized = minimized;
            inner.is_root = is_root;
        }
        window
    }

    pub fn inner(&self) -> std::sync::RwLockReadGuard<'_, InnerWindow> {
        self.inner.read().unwrap()
    }

    pub fn did_receive_focus(&self, window_manager: &mut WindowManager) {
        let focused_id = window_manager.focused_window;

        let my_id = self.inner().id;
        if focused_id.is_none_or(|id| id != my_id) {
            if window_manager.ffm_window_id.is_none_or(|id| id != my_id) {
                self.center_mouse(window_manager.main_cid);
            }
            window_manager.last_window = focused_id;
        }

        debug!("did_receive_focus: {} getting focus", my_id);
        window_manager.focused_window = Some(my_id);
        window_manager.focused_psn = self.inner().app.inner().psn.clone();
        window_manager.ffm_window_id = None;

        window_manager.reshuffle_around(self);
    }

    fn parent(main_conn: ConnID, window_id: WinID) -> Option<WinID> {
        let windows = create_array(vec![window_id], CFNumberType::SInt32Type)?;
        unsafe {
            let query = CFRetained::from_raw(SLSWindowQueryWindows(main_conn, windows.deref(), 1));
            let iterator =
                CFRetained::from_raw(SLSWindowQueryResultCopyWindows(query.deref().into()));
            if 1 == SLSWindowIteratorGetCount(iterator.deref())
                && SLSWindowIteratorAdvance(iterator.deref())
            {
                return Some(SLSWindowIteratorGetParentID(iterator.deref()));
            }
        }
        None
    }

    fn title(&self) -> Option<String> {
        let axtitle = CFString::from_static_str(kAXTitleAttribute);
        let title = get_attribute::<CFString>(&self.inner().element_ref, axtitle)?;
        Some(get_string_from_string(title.deref()))
    }

    fn role(&self) -> Option<String> {
        let axrole = CFString::from_static_str(kAXRoleAttribute);
        let role = get_attribute::<CFString>(&self.inner().element_ref, axrole)?;
        Some(get_string_from_string(role.deref()))
    }

    fn subrole(&self) -> Option<String> {
        let axrole = CFString::from_static_str(kAXSubroleAttribute);
        let role = get_attribute::<CFString>(&self.inner().element_ref, axrole)?;
        Some(get_string_from_string(role.deref()))
    }

    fn is_unknown(&self) -> bool {
        self.subrole()
            .is_some_and(|subrole| subrole.eq(kAXUnknownSubrole))
    }

    fn is_minimized(&self) -> bool {
        let axminimized = CFString::from_static_str(kAXMinimizedAttribute);
        get_attribute::<CFBoolean>(&self.inner().element_ref, axminimized)
            .map(|minimized| unsafe { CFBooleanGetValue(minimized.deref()) })
            .is_some_and(|minimized| minimized || self.inner().minimized)
    }

    fn is_root(&self) -> bool {
        let inner = self.inner();
        let cftype = inner.element_ref.as_ref();
        let axparent = CFString::from_static_str(kAXParentAttribute);
        get_attribute::<CFType>(&self.inner().element_ref, axparent)
            .is_some_and(|parent| !CFEqual(Some(parent.deref()), Some(cftype)))
    }

    fn is_real(&self) -> bool {
        let role = self.role().is_some_and(|role| role.eq(kAXWindowRole));
        role && self.subrole().is_some_and(|subrole| {
            [
                kAXStandardWindowSubrole,
                kAXFloatingWindowSubrole,
                kAXDialogSubrole,
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
        let point = CGPoint::new(x, y);
        unsafe {
            let position_ref = AXValueCreate(
                kAXValueTypeCGPoint,
                &point as *const CGPoint as *const c_void,
            );
            let position =
                AxuWrapperType::retain(position_ref).expect("Can't get positon from window!");
            AXUIElementSetAttributeValue(
                self.inner().element_ref.as_ptr(),
                CFString::from_static_str(kAXPositionAttribute).as_ref(),
                position.as_ref(),
            );
        }
        self.inner.write().unwrap().frame.origin = point;
    }

    pub fn resize(&self, width: f64, height: f64) {
        let size = CGSize::new(width, height);
        unsafe {
            let size_ref =
                AXValueCreate(kAXValueTypeCGSize, &size as *const CGSize as *const c_void);
            let position =
                AxuWrapperType::retain(size_ref).expect("Can't get positon from window!");
            AXUIElementSetAttributeValue(
                self.inner().element_ref.as_ptr(),
                CFString::from_static_str(kAXSizeAttribute).as_ref(),
                position.as_ref(),
            );
        }
        self.inner.write().unwrap().frame.size = size;
    }

    fn update_frame(&self) {
        let mut frame = CGRect::default();
        let mut position_ref: *mut CFType = null_mut();
        let mut size_ref: *mut CFType = null_mut();
        let window_ref = self.inner().element_ref.as_ptr();

        unsafe {
            AXUIElementCopyAttributeValue(
                window_ref,
                CFString::from_static_str(kAXPositionAttribute).as_ref(),
                &mut position_ref as *mut *mut CFType,
            );
            AXUIElementCopyAttributeValue(
                window_ref,
                CFString::from_static_str(kAXSizeAttribute).as_ref(),
                &mut size_ref as *mut *mut CFType,
            );
        };
        unsafe {
            if let Some(position) = AxuWrapperType::retain(position_ref) {
                AXValueGetValue(
                    position.as_ptr(),
                    kAXValueTypeCGPoint,
                    &mut frame.origin as *mut CGPoint as *mut c_void,
                );
            }
            if let Some(size) = AxuWrapperType::retain(size_ref) {
                AXValueGetValue(
                    size.as_ptr(),
                    kAXValueTypeCGSize,
                    &mut frame.size as *mut CGSize as *mut c_void,
                );
            }
            if CGRectEqualToRect(frame, self.inner().frame) {
                debug!("Debounced window resize: {}", self.inner().app.inner().name)
            } else {
                self.inner.write().unwrap().frame = frame;
            }
        }
    }

    fn make_key_window(&self) {
        let psn = self.inner().app.inner().psn.clone();
        let window_id = self.inner().id;
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
        unsafe { SLPSPostEventRecordTo(&psn, event_bytes.as_ptr() as *const c_void) };

        event_bytes[0x08] = 0x02;
        unsafe { SLPSPostEventRecordTo(&psn, event_bytes.as_ptr() as *const c_void) };
    }

    // const CPS_ALL_WINDOWS: u32 = 0x100;
    const CPS_USER_GENERATED: u32 = 0x200;
    // const CPS_NO_WINDOWS: u32 = 0x400;

    pub fn focus_without_raise(&self, window_manager: &WindowManager) {
        let psn = self.inner().app.inner().psn.clone();
        let window_id = self.inner().id;
        debug!("focus_window_without_raise: {window_id}");
        if window_manager.focused_psn == psn && window_manager.focused_window.is_some() {
            let mut event_bytes = [0u8; 0xf8];
            event_bytes[0x04] = 0xf8;
            event_bytes[0x08] = 0x0d;

            event_bytes[0x8a] = 0x02;
            let wid = window_manager.focused_window.unwrap().to_ne_bytes();
            event_bytes[0x3c..(0x3c + wid.len())].copy_from_slice(&wid);
            unsafe {
                SLPSPostEventRecordTo(
                    &window_manager.focused_psn,
                    event_bytes.as_ptr() as *const c_void,
                );
            }
            // @hack
            // Artificially delay the activation by 1ms. This is necessary because some
            // applications appear to be confused if both of the events appear instantaneously.
            thread::sleep(Duration::from_millis(20));

            event_bytes[0x8a] = 0x01;
            let wid = &window_id.to_ne_bytes();
            event_bytes[0x3c..(0x3c + wid.len())].copy_from_slice(wid);
            unsafe {
                SLPSPostEventRecordTo(&psn, event_bytes.as_ptr() as *const c_void);
            }
        }

        unsafe {
            _SLPSSetFrontProcessWithOptions(&psn, window_id, Self::CPS_USER_GENERATED);
        }
        self.make_key_window();
    }

    pub fn focus_with_raise(&self) {
        let psn = self.inner().app.inner().psn.clone();
        let window_id = self.inner().id;
        unsafe {
            _SLPSSetFrontProcessWithOptions(&psn, window_id, Self::CPS_USER_GENERATED);
        }
        self.make_key_window();
        let element_ref = self.inner().element_ref.as_ptr();
        let action = CFString::from_static_str(kAXRaiseAction);
        unsafe { AXUIElementPerformAction(element_ref, &action) };
    }

    fn observe(&self, app: &Application) -> bool {
        app.inner().handler.window_observe(self)
    }

    fn display_uuid(&self, cid: ConnID) -> Option<Retained<CFString>> {
        let window_id = self.inner().id;
        let uuid = unsafe {
            NonNull::new(SLSCopyManagedDisplayForWindow(cid, window_id) as *mut CFString)
                .and_then(|uuid| Retained::from_raw(uuid.as_ptr()))
        };
        uuid.or_else(|| {
            let mut frame = CGRect::default();
            unsafe {
                SLSGetWindowBounds(cid, window_id, &mut frame);
                NonNull::new(SLSCopyBestManagedDisplayForRect(cid, frame) as *mut CFString)
                    .and_then(|uuid| Retained::from_raw(uuid.as_ptr()))
            }
        })
    }

    fn display_id(&self, cid: ConnID) -> Option<u32> {
        let uuid = self.display_uuid(cid);
        uuid.and_then(|uuid| Display::id_from_uuid(uuid.into()))
    }

    pub fn center_mouse(&self, cid: ConnID) {
        // TODO: check for MouseFollowsFocus setting in WindowManager and also whether it's
        // overriden for individual window.

        let frame = self.inner().frame;
        let mut cursor = CGPoint::default();
        if unsafe { CGError::Success != SLSGetCurrentCursorLocation(cid, &mut cursor) } {
            warn!("center_mouse: Unable to get current cursor position.");
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
        if bounds.is_some_and(|bounds| unsafe { !CGRectContainsPoint(bounds, center) }) {
            return;
        }
        unsafe { CGWarpMouseCursorPosition(center) };
    }
}

impl Drop for InnerWindow {
    fn drop(&mut self) {
        ApplicationHandler::window_unobserve(self);
    }
}

pub struct WindowManager {
    pub tx: Sender<Event>,
    pub applications: HashMap<Pid, Application>,
    pub main_cid: ConnID,
    pub windows: HashMap<WinID, Window>,
    pub last_window: Option<WinID>,
    pub focused_window: Option<WinID>,
    pub focused_psn: ProcessSerialNumber,
    pub ffm_window_id: Option<WinID>,
    pub mission_control_is_active: bool,
    pub current_window: Option<Window>,
    pub down_location: CGPoint,
    pub displays: Vec<Display>,
}

impl WindowManager {
    pub fn new(tx: Sender<Event>, main_cid: ConnID) -> Self {
        let displays = Display::active_displays(main_cid);
        if displays.is_empty() {
            error!("Can not find any displays?!");
        }

        WindowManager {
            tx,
            applications: HashMap::new(),
            main_cid,
            windows: HashMap::new(),
            last_window: None,
            focused_window: None,
            focused_psn: ProcessSerialNumber::default(),
            ffm_window_id: None,
            mission_control_is_active: false,
            current_window: None,
            down_location: CGPoint::default(),
            displays,
        }
    }

    pub fn start(&mut self, process_manager: &mut ProcessManager) {
        autoreleasepool(|_| {
            for process in process_manager.processes.values_mut() {
                if process.is_observable() {
                    let app = Application::from_process(self.main_cid, process, self.tx.clone());
                    debug!("wm start: Application {} is observable", app.inner().name);

                    if app.observe() {
                        self.applications.insert(app.inner().pid, app.clone());
                        self.add_existing_application_windows(&app, 0);
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
            for (space, windows) in display.spaces.iter() {
                windows.write().unwrap().extend(
                    self.space_window_list_for_connection(vec![*space], None, false)
                        .unwrap_or_default()
                        .into_iter()
                        .flat_map(|window_id| self.find_window(window_id)),
                )
            }
        }

        if let Some(window) = self.focused_window() {
            self.last_window = Some(window.inner().id);
            self.focused_window = Some(window.inner().id);
            self.focused_psn = window.inner().app.inner().psn.clone();
        }
    }

    pub fn find_application(&self, pid: Pid) -> Option<Application> {
        self.applications.get(&pid).cloned()
    }

    pub fn find_window(&self, window_id: WinID) -> Option<Window> {
        self.windows.get(&window_id).cloned()
    }

    fn space_window_list_for_connection(
        &self,
        spaces: Vec<u64>,
        cid: Option<ConnID>,
        also_minimized: bool,
    ) -> Option<Vec<WinID>> {
        unsafe {
            let space_list_ref = create_array(spaces, CFNumberType::SInt64Type)?;

            let set_tags = 0i64;
            let clear_tags = 0i64;
            let options = if also_minimized { 0x7 } else { 0x2 };
            let window_list_ref =
                CFRetained::from_raw(NonNull::new(SLSCopyWindowsWithOptionsAndTags(
                    self.main_cid,
                    cid.unwrap_or(0),
                    space_list_ref.deref(),
                    options,
                    &set_tags,
                    &clear_tags,
                ))?);

            let count = CFArrayGetCount(window_list_ref.deref());
            if count == 0 {
                return None;
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
                            window_list.push(window.inner().id);
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
            Some(window_list)
        }
    }

    fn existing_application_window_list(&self, app: &Application) -> Option<Vec<WinID>> {
        let spaces: Vec<u64> = self
            .displays
            .iter()
            .flat_map(|display| display.spaces.keys().cloned().collect::<Vec<_>>())
            .collect();
        debug!("existing_application_window_list: spaces {spaces:?}");
        if spaces.is_empty() {
            return None;
        }
        self.space_window_list_for_connection(spaces, app.inner().connection, true)
    }

    fn bruteforce_windows(&mut self, app: &Application, window_list: &mut Vec<WinID>) {
        debug!(
            "bruteforce_windows: App {} has unresolved window on other desktops, bruteforcing them.",
            app.inner().name
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
            let data_ref =
                CFDataCreateMutable(None, BUFSIZE).expect("Unable to create mutable data.");
            CFDataIncreaseLength(data_ref.deref().into(), BUFSIZE);

            // uint8_t *data = CFDataGetMutableBytePtr(data_ref);
            // *(uint32_t *) (data + 0x0) = application->pid;
            // *(uint32_t *) (data + 0x8) = 0x636f636f;
            const MAGIC: u32 = 0x636f636f;
            let data = from_raw_parts_mut(
                CFDataGetMutableBytePtr(data_ref.deref().into()),
                BUFSIZE as usize,
            );
            let bytes = app.inner().pid.to_ne_bytes();
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
                let element_ref =
                    AxuWrapperType::retain(_AXUIElementCreateWithRemoteToken(data_ref.as_ref()));
                let window_id = element_ref
                    .as_ref()
                    .and_then(|element| ax_window_id(element.as_ptr()));
                let mut matched = false;

                //     if (element_wid != 0) {
                //         for (int i = 0; i < app_window_list_len; ++i) {
                //             if (app_window_list[i] == element_wid) {
                //                 matched = true;
                //                 ts_buf_del(app_window_list, i);
                //                 break;
                //             }
                //         }
                //     }
                if let Some(window_id) = window_id {
                    let index = window_list.iter().position(|&id| id == window_id);
                    matched = index.map(|idx| window_list.remove(idx)).is_some();
                }

                //     if (matched) {
                //         window_manager_create_and_add_window(sm, wm, application, element_ref, element_wid, false);
                //     } else {
                //         CFRelease(element_ref);
                //     }
                if matched {
                    debug!("bruteforce_windows: Found window {window_id:?}");
                    self.create_and_add_window(
                        app,
                        element_ref.unwrap(),
                        window_id.unwrap(),
                        false,
                    );
                }
            }
        }
    }

    fn add_existing_application_windows(&mut self, app: &Application, refresh_index: i32) -> bool {
        let mut result = false;

        let global_window_list = self.existing_application_window_list(app);
        if global_window_list
            .as_ref()
            .is_some_and(|list| list.is_empty())
        {
            warn!(
                "add_existing_application_windows: No existing windows for app {}",
                app.inner().name
            );
            return result;
        }
        let global_window_list = global_window_list.unwrap();
        info!(
            "add_existing_application_windows: App {} has global windows: {global_window_list:?}",
            app.inner().name
        );

        let window_list = app.window_list();
        let window_count = if let Some(window_list) = &window_list {
            unsafe { CFArrayGetCount(window_list) }
        } else {
            0
        };
        let mut empty_count = 0;
        if let Some(window_list) = window_list {
            for window_ref in get_array_values(window_list.deref()) {
                let window_id = ax_window_id(window_ref.as_ptr());

                //
                // @cleanup
                //
                // :Workaround
                //
                // NOTE(koekeishiya): The AX API appears to always include a single element for
                // Finder that returns an empty window id. This is likely the desktop window. Other
                // similar cases should be handled the same way; simply ignore the window when we
                // attempt to do an equality check to see if we have correctly discovered the
                // number of windows to track.

                if window_id.is_none() {
                    empty_count += 1;
                    continue;
                }
                let window_id = window_id.unwrap();

                if self.find_window(window_id).is_none() {
                    let window_ref = AxuWrapperType::retain(window_ref.as_ptr()).unwrap();
                    info!(
                        "add_existing_application_windows: Add window: {} {window_id}",
                        app.inner().name
                    );
                    self.create_and_add_window(app, window_ref, window_id, false);
                }
            }
        }

        if global_window_list.len() as isize == (window_count - empty_count) {
            if refresh_index != -1 {
                info!(
                    "add_existing_application_windows: All windows for {} are now resolved",
                    app.inner().name
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
                    "add_existing_application_windows: {} has windows that are not yet resolved",
                    app.inner().name
                );
                self.bruteforce_windows(app, &mut app_window_list);
            }
        }

        result
    }

    pub fn add_application_windows(&mut self, app: &Application) -> Vec<Window> {
        // TODO: maybe refactor this with add_existing_application_windows()
        app.window_list()
            .map(|window_list| {
                get_array_values::<accessibility_sys::__AXUIElement>(window_list.as_ref())
                    .flat_map(|element_ref| {
                        AxuWrapperType::retain(element_ref.as_ptr()).map(|element| {
                            ax_window_id(element.as_ptr()).and_then(|window_id| {
                                match self.find_window(window_id) {
                                    None => {
                                        self.create_and_add_window(app, element, window_id, true)
                                    }
                                    _ => None,
                                }
                            })
                        })
                    })
                    .flatten()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn create_and_add_window(
        &mut self,
        app: &Application,
        window_ref: CFRetained<AxuWrapperType>,
        window_id: WinID,
        _one_shot_rules: bool, // TODO: fix
    ) -> Option<Window> {
        let window = Window::new(app, window_ref, window_id);
        let title = window.title()?;
        let role = window.role()?;
        let subrole = window.subrole()?;
        info!(
            "create_and_add_window: {} {} {title} {role} {subrole}",
            window.inner().id,
            app.inner().name,
        );

        if window.is_unknown() {
            warn!(
                "create_and_add_window: Ignoring AXUnknown window {} {}",
                app.inner().name,
                window.inner().id
            );
            return None;
        }

        //
        // NOTE(koekeishiya): Attempt to track **all** windows.
        //

        if !window.observe(app) {
            warn!(
                "create_and_add_window: Could not observe window {} of {}",
                window.inner().id,
                app.inner().name
            );
            return None;
        }

        // if (window_manager_find_lost_focused_event(wm, window->id)) {
        //     event_loop_post(&g_event_loop, WINDOW_FOCUSED, (void *)(intptr_t) window->id, 0);
        //     window_manager_remove_lost_focused_event(wm, window->id);
        // }

        self.windows.insert(window_id, window.clone());
        Some(window)
    }

    fn focused_application(&self) -> Option<Application> {
        let mut psn = ProcessSerialNumber::default();
        let mut pinfo = ProcessInfo::default();
        unsafe {
            _SLPSGetFrontProcess(&mut psn);
            get_process_info(&psn, &mut pinfo);
        }
        self.find_application(pinfo.pid)
    }

    fn focused_window(&self) -> Option<Window> {
        let app = self.focused_application()?;
        let window_id = app.focused_window_id()?;
        self.find_window(window_id)
    }

    pub fn front_switched(&mut self, process: &mut Process) {
        let app = self.find_application(process.pid);
        if app.is_none() {
            warn!("process_front_switch: window_manager_add_lost_front_switched_event");
            // window_manager_add_lost_front_switched_event(&g_window_manager, process->pid);
            return;
        }
        let app = app.unwrap();
        debug!("process_front_switch: {}", app.inner().name);

        let focused_id = app.focused_window_id();
        if focused_id.is_none() {
            let focused_window = self
                .focused_window
                .and_then(|window_id| self.find_window(window_id));

            if focused_window.is_none() {
                warn!("window_manager_set_window_opacity");
            }

            self.last_window = self.focused_window;
            self.focused_window = None;
            self.focused_psn = app.inner().psn.clone();
            self.ffm_window_id = None;
            warn!("process_front_switch: reset focused window");
            return;
        }

        if let Some(window) = self.find_window(focused_id.unwrap()) {
            window.did_receive_focus(self);
        } else {
            warn!("process_front_switch: window_manager_add_lost_focused_event");
        }
    }

    pub fn window_created(&mut self, element_ref: CFRetained<AxuWrapperType>) {
        let window = ax_window_id(element_ref.as_ptr()).and_then(|window_id| {
            self.find_window(window_id).is_none().then_some(())?;
            let pid = ax_window_pid(&element_ref)?;
            let app = self.find_application(pid)?;
            self.create_and_add_window(&app, element_ref, window_id, true)
        });

        if let Some(window) = window {
            info!("window_created: {window:?}");

            if let Some(panel) = self.active_panel() {
                let previous = panel.read().ok().and_then(|panel| {
                    self.focused_window
                        .and_then(|id| panel.iter().position(|window| window.inner().id == id))
                });
                let inserted = previous.map(|prev| {
                    if let Ok(mut panel) = panel.write() {
                        panel.insert(prev + 1, window.clone());
                    }
                });
                if inserted.is_some() {
                    self.reshuffle_around(&window);
                }
            }

            window.did_receive_focus(self);
        }
    }

    pub fn window_destroyed(&mut self, window_id: WinID) {
        info!("window_destroyed: {window_id}");

        self.displays.iter().for_each(|display| {
            display.spaces.values().for_each(|windows| {
                windows
                    .write()
                    .unwrap()
                    .retain(|window| window.inner().id != window_id);
            });
        });

        let previous = self
            .focused_window
            .and_then(|previous| self.find_window(previous));
        if let Some(window) = previous {
            self.reshuffle_around(&window);
        }
    }

    pub fn window_moved(&self, _window_id: WinID) {
        // uint32_t window_id = (uint32_t)(intptr_t) context;
    }

    pub fn window_resized(&self, window_id: WinID) {
        if let Some(window) = self.find_window(window_id) {
            window.update_frame();
            self.reshuffle_around(&window);
        }
    }

    pub fn reshuffle_around(&self, window: &Window) {
        let active_space = self.active_panel();
        if active_space
            .as_ref()
            .is_none_or(|space| space.read().unwrap().is_empty())
        {
            error!("No workspace found.");
            return;
        }
        let active_space = active_space.unwrap();
        let active_space = active_space.write().unwrap();
        let focus_id = window.inner().id;
        let index = active_space
            .iter()
            .position(|window| window.inner().id == focus_id);
        if index.is_none() {
            return;
        }
        let index = index.unwrap();

        // Check if window needs to be fully exposed
        let mut frame = window.inner().frame;
        debug!("focus original position {frame:?}");
        let mut moved = false;
        if frame.origin.x + frame.size.width > SCREEN_WIDTH {
            debug!("Bumped window {} to the left", focus_id);
            frame.origin.x = SCREEN_WIDTH - frame.size.width;
            moved = true;
        } else if frame.origin.x < 0.0 {
            debug!("Bumped window {} to the right", focus_id);
            frame.origin.x = 0.0;
            moved = true;
        }
        if moved {
            window.reposition(frame.origin.x, frame.origin.y);
            trace!("focus resposition to {frame:?}");
        }

        // Shuffling windows to the right of the focus.
        let mut upper_left = frame.origin.x + frame.size.width;
        for window in &active_space[1 + index..] {
            let frame = window.inner().frame;
            trace!("right: frame: {frame:?}");
            // Check for window getting off screen.
            if upper_left > SCREEN_WIDTH - THRESHOLD {
                upper_left = SCREEN_WIDTH - THRESHOLD;
            }
            if frame.origin.x != upper_left {
                window.reposition(upper_left, frame.origin.y);
                trace!("right side moved to upper_left {upper_left}");
            }
            upper_left += frame.size.width;
        }

        // Shuffling windows to the left of the focus.
        let mut upper_left = frame.origin.x;
        trace!("focus upper_left {upper_left}");
        for window in active_space[0..index].iter().rev() {
            let frame = window.inner().frame;
            trace!("left: frame: {frame:?}");
            // Check for window getting off screen.
            if upper_left < THRESHOLD {
                upper_left = THRESHOLD;
            }
            upper_left -= frame.size.width;

            if frame.origin.x != upper_left {
                window.reposition(upper_left, frame.origin.y);
                trace!("left side moved to upper_left {upper_left}");
            }
        }
    }

    pub fn active_panel(&self) -> Option<WindowPane> {
        let display = Display::active_display_id(self.main_cid)
            .and_then(|id| self.displays.iter().find(|display| display.id == id));
        display.and_then(|display| {
            let space_id = display.active_display_space(self.main_cid);
            space_id
                .and_then(|space_id| display.spaces.get(&space_id))
                .cloned()
        })
    }
}

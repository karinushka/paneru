use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use bevy::prelude::*;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::CGDirectDisplayID;
use stdext::function_name;
use stdext::prelude::RwLockExt;
use tracing::{Level, debug, instrument};

use crate::errors::{Error, Result};
use crate::events::Event;
use crate::manager::{
    Application, ApplicationApi, Display, MockProcessApi, MockWindowManagerApi, Origin, ProcessApi,
    Size, Window, WindowApi, WindowManagerApi,
};
use crate::platform::{ConnID, Pid, WinID, WorkspaceId};
use crate::{platform::ProcessSerialNumber, util::AXUIWrapper};

use super::*;

/// A mock implementation of the `ApplicationApi` trait for testing purposes.
/// It internally holds an `InnerMockApplication` within an `Arc<RwLock>`.
#[derive(Clone, Debug)]
pub(crate) struct MockApplication {
    pub(crate) inner: Arc<RwLock<InnerMockApplication>>,
    pub(crate) name: String,
}

/// The inner state of `MockApplication`, containing process serial number, PID, and focused window ID.
#[derive(Debug)]
pub(crate) struct InnerMockApplication {
    pub(crate) psn: ProcessSerialNumber,
    pub(crate) pid: Pid,
    pub(crate) focused_id: Option<WinID>,
    pub(crate) bundle_id: String,
}

impl MockApplication {
    /// Creates a new `MockApplication` instance.
    #[instrument(level = Level::DEBUG, ret)]
    pub(crate) fn new(psn: ProcessSerialNumber, pid: Pid, bundle_id: String) -> Self {
        MockApplication {
            inner: Arc::new(RwLock::new(InnerMockApplication {
                psn,
                pid,
                focused_id: None,
                bundle_id,
            })),
            name: "test".to_string(),
        }
    }
}

impl ApplicationApi for MockApplication {
    /// Returns the PID of the mock application.
    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn pid(&self) -> Pid {
        self.inner.force_read().pid
    }

    /// Returns the `ProcessSerialNumber` of the mock application.
    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn psn(&self) -> ProcessSerialNumber {
        debug!("{}:", function_name!());
        self.inner.force_read().psn
    }

    /// Always returns `Some(0)` for the connection ID.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn connection(&self) -> Option<ConnID> {
        debug!("{}:", function_name!());
        Some(0)
    }

    /// Returns the currently focused window ID for the mock application.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` if a window is focused, otherwise `Err(Error::InvalidWindow)`.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn focused_window_id(&self) -> Result<WinID> {
        let id = self
            .inner
            .force_read()
            .focused_id
            .ok_or(Error::InvalidWindow);
        debug!("{}: {id:?}", function_name!());
        id
    }

    /// Always returns an empty vector of window lists for the mock application.
    fn window_list(&self) -> Vec<Window> {
        debug!("{}:", function_name!());
        vec![]
    }

    /// Always returns `Ok(true)` for observe operations on the mock application.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn observe(&mut self) -> Result<bool> {
        debug!("{}:", function_name!());
        Ok(true)
    }

    /// Always returns `Ok(true)` for observe window operations on the mock application.
    #[instrument(level = Level::DEBUG, skip_all, ret)]
    fn observe_window(&mut self, _window: &Window) -> Result<bool> {
        debug!("{}:", function_name!());
        Ok(true)
    }

    /// Does nothing for unobserve window operations on the mock application.
    #[instrument(level = Level::DEBUG, skip_all, ret)]
    fn unobserve_window(&mut self, _window: &Window) {
        debug!("{}:", function_name!());
    }

    /// Always returns `true`, indicating the mock application is frontmost.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn is_frontmost(&self) -> bool {
        debug!("{}:", function_name!());
        true
    }

    /// Returns the bundle identifier of the application.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn bundle_id(&self) -> Option<String> {
        debug!("{}:", function_name!());
        Some(self.inner.force_read().bundle_id.to_owned())
    }

    fn name(&self) -> &str {
        &self.name
    }
}

pub(crate) fn create_mock_process(psn: ProcessSerialNumber) -> MockProcessApi {
    let mut mp = MockProcessApi::new();

    mp.expect_is_observable().returning(|| true);
    mp.expect_name().return_const("test".to_string());
    mp.expect_pid().return_const(TEST_PROCESS_ID);
    mp.expect_psn().return_const(psn);
    mp.expect_application().return_const(None);
    mp.expect_ready().return_const(true);

    mp
}

#[derive(Clone)]
pub(crate) struct MockWindowManagerState {
    inner: Arc<RwLock<MockWindowManagerStateInner>>,
}

struct MockWindowManagerStateInner {
    windows: TestWindowSpawner,
    workspaces: Vec<WorkspaceId>,
    window_ids: Vec<WinID>,
    visible_windows: Vec<WinID>,
}

impl MockWindowManagerState {
    pub(crate) fn new(
        windows: TestWindowSpawner,
        workspaces: Vec<WorkspaceId>,
        window_ids: Vec<WinID>,
        visible_windows: Vec<WinID>,
    ) -> Self {
        Self {
            inner: Arc::new(RwLock::new(MockWindowManagerStateInner {
                windows,
                workspaces,
                window_ids,
                visible_windows,
            })),
        }
    }

    pub(crate) fn windows(&self, workspace_id: WorkspaceId) -> Vec<Window> {
        (self.inner.force_read().windows)(workspace_id)
    }

    pub(crate) fn workspaces(&self) -> Vec<WorkspaceId> {
        self.inner.force_read().workspaces.clone()
    }

    pub(crate) fn window_ids(&self) -> Vec<WinID> {
        self.inner.force_read().window_ids.clone()
    }

    pub(crate) fn visible_windows(&self) -> Vec<WinID> {
        self.inner.force_read().visible_windows.clone()
    }
}

pub(crate) fn create_mock_window_manager(state: MockWindowManagerState) -> MockWindowManagerApi {
    let mut wm = MockWindowManagerApi::new();

    wm.expect_active_display_id()
        .returning(|| Ok(TEST_DISPLAY_ID));

    let state_clone = state.clone();
    wm.expect_active_display_space()
        .with(mockall::predicate::eq(TEST_DISPLAY_ID))
        .returning(move |_| Ok(state_clone.workspaces()[0]));

    let state_clone = state.clone();
    wm.expect_present_displays().returning(move || {
        let display = Display::new(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            TEST_MENUBAR_HEIGHT,
        );
        vec![(display, state_clone.workspaces())]
    });

    let state_clone = state.clone();
    let state_clone2 = state.clone();
    wm.expect_find_existing_application_windows()
        .withf(move |app, spaces| {
            debug!(
                "{}: app {} spaces {:?}",
                function_name!(),
                app.pid(),
                spaces
            );
            let valid_space = spaces
                .into_iter()
                .all(|id| state_clone.workspaces().contains(id));
            app.pid() == 1 && valid_space
        })
        .returning(move |_app, spaces| {
            let windows = spaces
                .iter()
                .flat_map(|workspace_id| state_clone2.windows(*workspace_id))
                .collect::<Vec<_>>();
            Ok((windows, vec![]))
        });

    let state_clone = state.clone();
    wm.expect_windows_in_workspace()
        .with(mockall::predicate::eq(TEST_WORKSPACE_ID))
        .returning(move |_workspace_id| {
            let mut ids = state_clone.window_ids();
            ids.reverse();
            debug!("{}:", function_name!());
            Ok(ids)
        });

    wm.expect_warp_mouse().return_const(());

    wm.expect_cursor_position().return_const(None);

    wm.expect_get_associated_windows().return_const(vec![]);

    let state_clone = state.clone();
    wm.expect_windows_on_screen()
        .returning(move || Some(state_clone.visible_windows()));

    wm.expect_find_window_at_point().returning(|point| {
        debug!("find_window_at_point: {point:?}");
        Ok(0)
    });

    wm
}

/// A mock implementation of the `WindowApi` trait for testing purposes.
#[derive(Debug)]
pub(crate) struct MockWindow {
    pub(crate) id: WinID,
    pub(crate) frame: IRect,
    pub(crate) horizontal_padding: i32,
    pub(crate) vertical_padding: i32,
    pub(crate) app: MockApplication,
    pub(crate) event_queue: EventQueue,
    pub(crate) minimized: bool,
    pub(crate) title: String,
    pub(crate) identifier: String,
    pub(crate) role: String,
    pub(crate) subrole: String,
    pub(crate) ignored_repositions: Arc<AtomicUsize>,
    pub(crate) metadata_reads: Option<Arc<AtomicUsize>>,
}

impl WindowApi for MockWindow {
    /// Returns the ID of the mock window.
    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn id(&self) -> WinID {
        self.id
    }

    /// Returns the frame (`CGRect`) of the mock window.
    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn frame(&self) -> IRect {
        self.frame
    }

    /// Returns a dummy `CFRetained<AXUIWrapper>` for the mock window's accessibility element.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn element(&self) -> Option<CFRetained<AXUIWrapper>> {
        debug!("{}:", function_name!());
        None
    }

    /// Returns the title of the mock window.
    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn title(&self) -> Result<String> {
        if let Some(reads) = &self.metadata_reads {
            reads.fetch_add(1, Ordering::Relaxed);
        }
        Ok(self.title.clone())
    }

    /// Returns the identifier of the mock window.
    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn identifier(&self) -> Result<String> {
        if let Some(reads) = &self.metadata_reads {
            reads.fetch_add(1, Ordering::Relaxed);
        }
        Ok(self.identifier.clone())
    }

    /// Always returns `Ok(true)` for valid role.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn child_role(&self) -> Result<bool> {
        debug!("{}:", function_name!());
        Ok(true)
    }

    /// Returns the role of the mock window.
    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn role(&self) -> Result<String> {
        Ok(self.role.clone())
    }

    /// Returns the subrole of the mock window.
    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn subrole(&self) -> Result<String> {
        Ok(self.subrole.clone())
    }

    /// Repositions the mock window's frame to the given coordinates.
    #[instrument(level = Level::DEBUG, skip(self))]
    fn reposition(&mut self, origin: Origin) {
        debug!("{}: id {} to {origin}", function_name!(), self.id);
        if self.ignored_repositions.load(Ordering::SeqCst) > 0 {
            self.ignored_repositions.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        let size = self.frame.size();
        self.frame.min = origin;
        self.frame.max = origin + size;
    }

    /// Resizes the mock window's frame to the given dimensions.
    #[instrument(level = Level::DEBUG, skip(self))]
    fn resize(&mut self, size: Size) {
        debug!("{}: id {} to {size}", function_name!(), self.id);
        self.frame.max = self.frame.min + size;
    }

    /// Always returns `Ok(())` for updating the frame.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn update_frame(&mut self) -> Result<IRect> {
        debug!("{}:", function_name!());
        Ok(self.frame)
    }

    /// Prints a debug message for focus without raise.
    #[instrument(level = Level::DEBUG, skip_all)]
    fn focus_without_raise(
        &self,
        _psn: ProcessSerialNumber,
        currently_focused: &Window,
        _ocused_psn: ProcessSerialNumber,
    ) {
        debug!(
            "{}: id {} {}",
            function_name!(),
            self.id,
            currently_focused.id()
        );
    }

    /// Prints a debug message for focus with raise and updates the mock application's focused ID.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn focus_with_raise(&self, psn: ProcessSerialNumber) {
        debug!("{}: id {}", function_name!(), self.id);
        self.event_queue
            .write()
            .unwrap()
            .push(Event::ApplicationFrontSwitched { psn });
        self.event_queue
            .write()
            .unwrap()
            .push(Event::WindowFocused { window_id: self.id });
        self.app.inner.force_write().focused_id = Some(self.id);
    }

    #[instrument(level = Level::DEBUG, skip(self))]
    fn raise_without_focus(&self) {
        debug!("{}: id {}", function_name!(), self.id);
    }

    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn pid(&self) -> Result<Pid> {
        Ok(TEST_PROCESS_ID)
    }

    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn set_padding(&mut self, padding: crate::manager::WindowPadding) {
        match padding {
            crate::manager::WindowPadding::Vertical(padding) => {
                let delta = padding - self.vertical_padding;
                self.frame.min.y -= delta;
                self.frame.max.y += delta;
                self.vertical_padding = padding;
            }
            crate::manager::WindowPadding::Horizontal(padding) => {
                let delta = padding - self.horizontal_padding;
                self.frame.min.x -= delta;
                self.frame.max.x += delta;
                self.horizontal_padding = padding;
            }
        }
    }

    fn horizontal_padding(&self) -> i32 {
        self.horizontal_padding
    }

    fn vertical_padding(&self) -> i32 {
        self.vertical_padding
    }

    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn is_minimized(&self) -> bool {
        self.minimized
    }

    fn is_full_screen(&self) -> bool {
        false
    }

    fn border_radius(&self) -> Option<f64> {
        None
    }
}

impl MockWindow {
    /// Creates a new `MockWindow` instance.
    pub(crate) fn new(
        id: WinID,
        frame: IRect,
        event_queue: EventQueue,
        app: MockApplication,
    ) -> Self {
        MockWindow {
            id,
            frame,
            horizontal_padding: 0,
            vertical_padding: 0,
            app,
            event_queue,
            minimized: false,
            title: String::new(),
            identifier: String::new(),
            role: "AXWindow".to_string(),
            subrole: "AXStandardWindow".to_string(),
            ignored_repositions: Arc::default(),
            metadata_reads: None,
        }
    }

    pub(crate) fn with_ignored_repositions(
        mut self,
        ignored_repositions: Arc<AtomicUsize>,
    ) -> Self {
        self.ignored_repositions = ignored_repositions;
        self
    }
}

/// Mock window manager with two displays of different heights.
pub(crate) struct TwoDisplayMock {
    pub(crate) windows: TestWindowSpawner,
    pub(crate) active_display: Arc<std::sync::atomic::AtomicU32>,
}

impl std::fmt::Debug for TwoDisplayMock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TwoDisplayMock").finish()
    }
}

pub(crate) const EXT_DISPLAY_ID: u32 = 2;
pub(crate) const EXT_WORKSPACE_ID: u64 = 20;
pub(crate) const EXT_DISPLAY_WIDTH: i32 = 1920;
pub(crate) const EXT_DISPLAY_HEIGHT: i32 = 1200;

impl WindowManagerApi for TwoDisplayMock {
    fn new_application(&self, process: &dyn ProcessApi) -> Result<Application> {
        Ok(Application::new(Box::new(MockApplication {
            inner: Arc::new(RwLock::new(InnerMockApplication {
                psn: process.psn(),
                pid: process.pid(),
                focused_id: None,
                bundle_id: "test".to_string(),
            })),
            name: "test".to_string(),
        })))
    }

    fn get_associated_windows(&self, _window_id: WinID) -> Vec<WinID> {
        vec![]
    }

    fn present_displays(&self) -> Vec<(Display, Vec<WorkspaceId>)> {
        // External display sits above the internal one.
        let ext = Display::new(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            TEST_MENUBAR_HEIGHT,
        );
        let int = Display::new(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            TEST_MENUBAR_HEIGHT,
        );
        vec![
            (ext, vec![EXT_WORKSPACE_ID]),
            (int, vec![TEST_WORKSPACE_ID]),
        ]
    }

    fn active_display_id(&self) -> Result<u32> {
        Ok(self.active_display.load(Ordering::Relaxed))
    }

    fn active_display_space(&self, display_id: CGDirectDisplayID) -> Result<WorkspaceId> {
        if display_id == EXT_DISPLAY_ID {
            Ok(EXT_WORKSPACE_ID)
        } else {
            Ok(TEST_WORKSPACE_ID)
        }
    }

    fn is_fullscreen_space(&self, _display_id: CGDirectDisplayID) -> bool {
        false
    }

    fn warp_mouse(&self, _origin: Origin) {}

    fn find_existing_application_windows(
        &self,
        _app: &mut Application,
        spaces: &[WorkspaceId],
    ) -> Result<(Vec<Window>, Vec<WinID>)> {
        let windows = spaces
            .iter()
            .flat_map(|workspace_id| (self.windows)(*workspace_id))
            .collect();
        Ok((windows, vec![]))
    }

    fn find_window_at_point(&self, _point: &CGPoint) -> Result<WinID> {
        Ok(0)
    }

    fn windows_in_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<WinID>> {
        Ok((self.windows)(workspace_id)
            .iter()
            .map(|w| w.id())
            .collect())
    }

    fn quit(&self) -> Result<()> {
        Ok(())
    }

    fn setup_config_watcher(&self, _path: &std::path::Path) -> Result<Box<dyn notify::Watcher>> {
        todo!()
    }

    fn cursor_position(&self) -> Option<CGPoint> {
        None
    }

    fn dim_windows(&self, _windows: &[WinID], _level: f32) {}

    fn windows_on_screen(&self) -> Option<Vec<WinID>> {
        None
    }
}

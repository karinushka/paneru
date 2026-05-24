use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};

use bevy::prelude::*;
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::CGDirectDisplayID;
use stdext::function_name;
use stdext::prelude::RwLockExt;
use tracing::debug;

use crate::errors::{Error, Result};
use crate::events::Event;
use crate::manager::app::MockApplicationApi;
use crate::manager::{
    Application, Display, MockProcessApi, MockWindowApi, MockWindowManagerApi, Origin, ProcessApi,
    Window, WindowManagerApi,
};
use crate::platform::ProcessSerialNumber;
use crate::platform::{Pid, WinID, WorkspaceId};

use super::*;

#[derive(Clone)]
pub(crate) struct MockAppState {
    inner: Arc<RwLock<InnerMockAppState>>,
    name: String,
}

struct InnerMockAppState {
    psn: ProcessSerialNumber,
    pid: Pid,
    focused_id: Option<WinID>,
    bundle_id: String,
    window_manager_state: Option<MockWindowManagerState>,
}

pub(crate) fn create_mock_application(app_state: MockAppState) -> Application {
    let mut ma = MockApplicationApi::new();

    let pid = app_state.pid();
    let psn = app_state.psn();
    ma.expect_pid().return_const(pid);
    ma.expect_psn().return_const(psn);

    ma.expect_observe().returning(|| Ok(true));

    ma.expect_observe_window().returning(|_| Ok(true));

    let app = app_state.clone();
    ma.expect_focused_window_id().returning(move || {
        app.inner
            .force_read()
            .focused_id
            .ok_or(Error::InvalidWindow)
    });

    let bundle = app_state.bundle_id();
    ma.expect_bundle_id().return_const(bundle);

    ma.expect_is_frontmost().return_const(true);

    ma.expect_name().return_const(app_state.name);

    ma.expect_unobserve_window().return_const(());

    ma.expect_connection().return_const(Some(0));

    Application::new(Box::new(ma))
}

impl MockAppState {
    /// Creates a new `MockApplication` instance.
    pub(crate) fn new(psn: ProcessSerialNumber, pid: Pid, bundle_id: String) -> Self {
        MockAppState {
            inner: Arc::new(RwLock::new(InnerMockAppState {
                psn,
                pid,
                focused_id: None,
                bundle_id,
                window_manager_state: None,
            })),
            name: "test".to_string(),
        }
    }

    pub(crate) fn set_window_manager_state(&self, window_manager_state: MockWindowManagerState) {
        self.inner.force_write().window_manager_state = Some(window_manager_state);
    }

    fn pid(&self) -> Pid {
        self.inner.force_read().pid
    }

    fn psn(&self) -> ProcessSerialNumber {
        self.inner.force_read().psn
    }

    fn bundle_id(&self) -> Option<String> {
        Some(self.inner.force_read().bundle_id.to_owned())
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

pub(crate) fn create_mock_window(
    id: WinID,
    frame: IRect,
    event_queue: EventQueue,
    app: MockAppState,
) -> Window {
    let mut mw = MockWindowApi::new();

    let state = Arc::new(RwLock::new(MockWindow::new(
        frame,
        event_queue,
        app.clone(),
    )));

    mw.expect_id().return_const(id);
    mw.expect_element().return_const(None);
    mw.expect_title()
        .return_const(Ok("test window".to_string()));
    mw.expect_identifier()
        .return_const(Ok("testid".to_string()));
    mw.expect_child_role().return_const(Ok(true));
    mw.expect_role().return_const(Ok("testrole".to_string()));
    mw.expect_subrole()
        .return_const(Ok("testsubrole".to_string()));
    let pid = app.pid();
    mw.expect_pid().return_const(Ok(pid));

    let resize_state = state.clone();
    mw.expect_resize().returning(move |size| {
        if let Ok(mut lock) = resize_state.write() {
            lock.frame.max = lock.frame.min + size;
        }
        ()
    });
    let reposition_state = state.clone();
    mw.expect_reposition().returning(move |origin| {
        if let Ok(mut lock) = reposition_state.write() {
            let size = lock.frame.size();
            lock.frame.min = origin;
            lock.frame.max = origin + size;
        }
        ()
    });
    let update_frame_state = state.clone();
    mw.expect_update_frame().returning(move || {
        if let Ok(lock) = update_frame_state.read() {
            return Ok(lock.frame);
        } else {
            return Err(Error::InvalidWindow);
        }
    });

    let frame_state = state.clone();
    mw.expect_frame()
        .returning(move || frame_state.read().unwrap().frame);

    let raise_state = state.clone();
    mw.expect_focus_with_raise().returning(move |psn| {
        if let Ok(lock) = raise_state.write() {
            lock.event_queue
                .write()
                .unwrap()
                .push(Event::ApplicationFrontSwitched { psn });
            lock.event_queue
                .write()
                .unwrap()
                .push(Event::WindowFocused { window_id: id });
            lock.app.inner.force_write().focused_id = Some(id);
        }
    });
    mw.expect_raise_without_focus().return_const(());
    mw.expect_focus_without_raise().return_const(());

    let hpad_state = state.clone();
    mw.expect_horizontal_padding().returning(move || {
        hpad_state
            .read()
            .map(|lock| lock.horizontal_padding)
            .unwrap_or_default()
    });
    let vpad_state = state.clone();
    mw.expect_vertical_padding().returning(move || {
        vpad_state
            .read()
            .map(|lock| lock.vertical_padding)
            .unwrap_or_default()
    });

    let pad_state = state.clone();
    mw.expect_set_padding().returning(move |padding| {
        if let Ok(mut lock) = pad_state.write() {
            match padding {
                crate::manager::WindowPadding::Vertical(padding) => {
                    let delta = padding - lock.vertical_padding;
                    lock.frame.min.y -= delta;
                    lock.frame.max.y += delta;
                    lock.vertical_padding = padding;
                }
                crate::manager::WindowPadding::Horizontal(padding) => {
                    let delta = padding - lock.horizontal_padding;
                    lock.frame.min.x -= delta;
                    lock.frame.max.x += delta;
                    lock.horizontal_padding = padding;
                }
            }
        }
    });

    let mini_state = state.clone();
    mw.expect_is_minimized().returning(move || {
        mini_state
            .read()
            .map(|lock| lock.minimized)
            .is_ok_and(|minimized| minimized)
    });

    mw.expect_is_full_screen().return_const(false);
    mw.expect_border_radius().return_const(None);

    Window::new(Box::new(mw))
}

/// A mock implementation of the `WindowApi` trait for testing purposes.
struct MockWindow {
    pub(crate) frame: IRect,
    pub(crate) horizontal_padding: i32,
    pub(crate) vertical_padding: i32,
    pub(crate) app: MockAppState,
    pub(crate) event_queue: EventQueue,
    pub(crate) minimized: bool,
}

impl MockWindow {
    /// Creates a new `MockWindow` instance.
    pub(crate) fn new(frame: IRect, event_queue: EventQueue, app: MockAppState) -> Self {
        MockWindow {
            frame,
            horizontal_padding: 0,
            vertical_padding: 0,
            app,
            event_queue,
            minimized: false,
        }
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
        let app_state = MockAppState::new(process.psn(), process.pid(), "test".to_string());
        Ok(create_mock_application(app_state))
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

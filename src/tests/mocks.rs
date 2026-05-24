use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};

use bevy::prelude::*;
use objc2_core_graphics::CGDirectDisplayID;
use stdext::prelude::RwLockExt;

use crate::errors::Error;
use crate::events::Event;
use crate::manager::app::MockApplicationApi;
use crate::manager::{
    Application, Display, MockProcessApi, MockWindowApi, MockWindowManagerApi, Window,
};
use crate::platform::ProcessSerialNumber;
use crate::platform::{Pid, WinID, WorkspaceId};

use super::*;

/// Data for a mocked application.
struct MockAppData {
    psn: ProcessSerialNumber,
    bundle_id: String,
    name: String,
    focused_window_id: Option<WinID>,
}

/// Data for a mocked window.
#[derive(Default)]
struct MockWindowData {
    id: WinID,
    pid: Pid,
    frame: IRect,
    title: String,
    minimized: bool,
    workspace_id: WorkspaceId,
    visible: bool,
}

/// Data for a mocked display.
struct MockDisplayData {
    id: u32,
    bounds: IRect,
    workspaces: Vec<WorkspaceId>,
}

/// The internal state of our "Virtual macOS".
struct MockStateInner {
    apps: HashMap<Pid, MockAppData>,
    windows: HashMap<WinID, MockWindowData>,
    displays: HashMap<u32, MockDisplayData>,
    active_display_id: u32,
    event_queue: VecDeque<Event>,
}

#[derive(Clone)]
pub struct MockState {
    inner: Arc<RwLock<MockStateInner>>,
}

impl MockState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(MockStateInner {
                apps: HashMap::new(),
                windows: HashMap::new(),
                displays: HashMap::new(),
                active_display_id: 0,
                event_queue: VecDeque::new(),
            })),
        }
    }

    pub(crate) fn window_visible(&self, window_id: WinID, visible: bool) {
        let mut state = self.inner.force_write();
        let window = state.windows.get_mut(&window_id).expect("finding window");
        window.visible = visible;
    }

    // --- OS Behavior Methods ---

    pub fn spawn_app(&self, pid: Pid, bundle_id: &str, name: &str) {
        let mut inner = self.inner.force_write();
        inner.apps.insert(
            pid,
            MockAppData {
                psn: ProcessSerialNumber {
                    high: 0,
                    low: pid as u32,
                },
                bundle_id: bundle_id.to_string(),
                name: name.to_string(),
                focused_window_id: None,
            },
        );
    }

    pub fn spawn_window(
        &self,
        pid: Pid,
        workspace_id: WorkspaceId,
        id: WinID,
        frame: IRect,
    ) -> Window {
        let mut inner = self.inner.force_write();
        inner.windows.insert(
            id,
            MockWindowData {
                id,
                pid,
                frame,
                title: format!("Window {id}"),
                minimized: false,
                workspace_id,
                visible: true,
                ..default()
            },
        );
        self.create_window(id)
    }

    pub fn focus_window(&self, id: WinID) {
        let mut inner = self.inner.force_write();
        if let Some(win) = inner.windows.get(&id) {
            let pid = win.pid;
            if let Some(app) = inner.apps.get_mut(&pid) {
                app.focused_window_id = Some(id);
                let psn = app.psn;
                inner
                    .event_queue
                    .push_back(Event::ApplicationFrontSwitched { psn });
                inner
                    .event_queue
                    .push_back(Event::WindowFocused { window_id: id });
            }
        }
    }

    pub fn add_display(&mut self, id: u32, bounds: IRect, workspaces: Vec<WorkspaceId>) {
        let mut inner = self.inner.force_write();
        if inner.displays.is_empty() {
            inner.active_display_id = id;
        }
        inner.displays.insert(
            id,
            MockDisplayData {
                id,
                bounds,
                workspaces,
            },
        );
    }

    pub fn active_display(&self) -> CGDirectDisplayID {
        self.inner.force_read().active_display_id
    }

    pub fn drain_events(&self) -> Vec<Event> {
        let mut inner = self.inner.force_write();
        inner.event_queue.drain(..).collect()
    }

    // --- Mock Factory Methods ---

    pub fn create_window(&self, id: WinID) -> Window {
        let mut mw = MockWindowApi::new();

        mw.expect_id().return_const(id);

        let s = self.clone();
        mw.expect_pid().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.pid)
                .ok_or(Error::InvalidWindow)
        });

        let s = self.clone();
        mw.expect_frame().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.frame)
                .unwrap_or_default()
        });

        let s = self.clone();
        mw.expect_resize().returning(move |size| {
            let mut inner = s.inner.force_write();
            if let Some(w) = inner.windows.get_mut(&id) {
                w.frame.max = w.frame.min + size;
            }
        });

        let s_move = self.clone();
        mw.expect_reposition().returning(move |origin| {
            let mut inner = s_move.inner.force_write();
            if let Some(w) = inner.windows.get_mut(&id) {
                let size = w.frame.size();
                w.frame.min = origin;
                w.frame.max = origin + size;
            }
        });

        let s = self.clone();
        mw.expect_focus_with_raise().returning(move |_psn| {
            s.focus_window(id);
        });

        let s = self.clone();
        mw.expect_title().returning(move || {
            Ok(s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.title.clone())
                .unwrap_or_default())
        });

        let s = self.clone();
        mw.expect_is_minimized().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.minimized)
                .unwrap_or_default()
        });

        let s = self.clone();
        mw.expect_update_frame().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.frame)
                .ok_or(Error::InvalidWindow)
        });

        // Fill in remaining defaults
        mw.expect_element().return_const(None);
        mw.expect_identifier()
            .return_const(Ok("testid".to_string()));
        mw.expect_role().return_const(Ok("AXWindow".to_string()));
        mw.expect_subrole()
            .return_const(Ok("AXStandardWindow".to_string()));
        mw.expect_child_role().return_const(Ok(false));
        mw.expect_raise_without_focus().return_const(());
        mw.expect_focus_without_raise().return_const(());
        mw.expect_horizontal_padding().return_const(0);
        mw.expect_vertical_padding().return_const(0);
        mw.expect_set_padding().return_const(());
        mw.expect_is_full_screen().return_const(false);
        mw.expect_border_radius().return_const(None);

        Window::new(Box::new(mw))
    }

    pub fn create_application(&self, pid: Pid) -> Application {
        let mut ma = MockApplicationApi::new();
        let s = self.clone();

        ma.expect_pid().return_const(pid);
        ma.expect_psn()
            .returning(move || s.inner.force_read().apps.get(&pid).map(|a| a.psn).unwrap());

        let s = self.clone();
        ma.expect_focused_window_id().returning(move || {
            s.inner
                .force_read()
                .apps
                .get(&pid)
                .and_then(|a| a.focused_window_id)
                .ok_or(Error::InvalidWindow)
        });

        let s = self.clone();
        ma.expect_bundle_id().returning(move || {
            s.inner
                .force_read()
                .apps
                .get(&pid)
                .map(|a| a.bundle_id.clone())
        });

        let name = self
            .inner
            .force_read()
            .apps
            .get(&pid)
            .map(|a| a.name.clone())
            .unwrap();
        ma.expect_name().return_const(name);

        ma.expect_observe().returning(|| Ok(true));
        ma.expect_observe_window().returning(|_| Ok(true));
        ma.expect_unobserve_window().return_const(());
        ma.expect_is_frontmost().return_const(true);
        ma.expect_connection().return_const(Some(0));

        Application::new(Box::new(ma))
    }

    pub fn create_window_manager(&self) -> MockWindowManagerApi {
        let mut wm = MockWindowManagerApi::new();

        let s = self.clone();
        wm.expect_active_display_id()
            .returning(move || Ok(s.inner.force_read().active_display_id));

        let s = self.clone();
        wm.expect_active_display_space().returning(move |id| {
            s.inner
                .force_read()
                .displays
                .get(&id)
                .map(|d| d.workspaces[0])
                .ok_or(Error::InvalidWindow)
        });

        let s = self.clone();
        wm.expect_present_displays().returning(move || {
            s.inner
                .force_read()
                .displays
                .values()
                .map(|d| {
                    (
                        Display::new(d.id, d.bounds, TEST_MENUBAR_HEIGHT),
                        d.workspaces.clone(),
                    )
                })
                .collect()
        });

        let s = self.clone();
        wm.expect_find_existing_application_windows()
            .returning(move |app, spaces| {
                let pid = app.pid();
                let windows = s
                    .inner
                    .force_read()
                    .windows
                    .values()
                    .filter_map(|w| {
                        (w.pid == pid && spaces.contains(&w.workspace_id))
                            .then_some(s.create_window(w.id))
                    })
                    .collect::<Vec<_>>();
                Ok((windows, vec![]))
            });

        let s = self.clone();
        wm.expect_windows_in_workspace()
            .returning(move |workspace_id| {
                let mut windows = s
                    .inner
                    .force_read()
                    .windows
                    .values()
                    .filter_map(|w| (w.workspace_id == workspace_id).then_some(w.id))
                    .collect::<Vec<_>>();
                // Sort the windows to keep the tests consistent
                windows.sort();
                Ok(windows)
            });

        let s = self.clone();
        wm.expect_windows_on_screen().returning(move || {
            let windows = s
                .inner
                .force_read()
                .windows
                .iter()
                .filter_map(|(id, window)| window.visible.then_some(id))
                .cloned()
                .collect::<Vec<_>>();
            Some(windows)
        });

        wm.expect_warp_mouse().return_const(());
        wm.expect_cursor_position().return_const(None);
        wm.expect_get_associated_windows().return_const(vec![]);
        wm.expect_find_window_at_point().return_const(Ok(0));

        wm
    }

    pub fn create_process(&self, pid: Pid) -> MockProcessApi {
        let mut mp = MockProcessApi::new();
        let s = self.clone();

        let name = self
            .inner
            .force_read()
            .apps
            .get(&pid)
            .map(|a| a.name.clone())
            .unwrap();
        mp.expect_name().return_const(name);

        mp.expect_pid().return_const(pid);
        mp.expect_psn()
            .returning(move || s.inner.force_read().apps.get(&pid).map(|a| a.psn).unwrap());
        mp.expect_is_observable().returning(|| true);
        mp.expect_application().return_const(None);
        mp.expect_ready().return_const(true);

        mp
    }
}

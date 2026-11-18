use stdext::prelude::RwLockExt;

use super::mocks::MockState;
use crate::events::Event;
use crate::manager::spaces::{
    AssignMode, DesktopBridgeApi, RawDisplaySpaces, SpaceCensus, SpaceError, SpaceErrorKind,
    SpaceId, SpaceKind, WindowServerMeta,
};
use crate::platform::{Pid, WinID, WorkspaceId};

pub(crate) struct MockDesktopBridge(pub(crate) MockState);

#[allow(
    dead_code,
    reason = "test helpers exercised across desktop test suites"
)]
impl MockState {
    pub(crate) fn create_desktop_bridge(&self) -> MockDesktopBridge {
        MockDesktopBridge(self.clone())
    }

    pub(crate) fn add_space_on_display(
        &self,
        display_id: u32,
        workspace_id: WorkspaceId,
        emit_created: bool,
    ) {
        let mut inner = self.inner.force_write();
        if let Some(display) = inner.displays.get_mut(&display_id)
            && !display.workspaces.contains(&workspace_id)
        {
            display.workspaces.push(workspace_id);
        }
        if emit_created {
            inner.event_queue.push_back(Event::SpaceCreated {
                space_id: workspace_id,
            });
        }
    }

    pub(crate) fn set_window_sticky(&self, window_id: WinID, sticky: bool) {
        let mut inner = self.inner.force_write();
        let all_desktops: Vec<WorkspaceId> = inner
            .displays
            .values()
            .flat_map(|d| d.workspaces.iter().copied())
            .filter(|ws| !inner.fullscreen_spaces.contains(ws))
            .collect();
        if let Some(win) = inner.windows.get_mut(&window_id) {
            win.sticky = sticky;
            if sticky {
                for ws in all_desktops {
                    if ws != win.workspace_id {
                        win.extra_workspaces.insert(ws);
                    }
                }
            } else {
                win.extra_workspaces.clear();
            }
        }
    }

    pub(crate) fn set_fullscreen_owner(&self, space_id: WorkspaceId, owner_wid: WinID) {
        let mut inner = self.inner.force_write();
        inner.fullscreen_spaces.insert(space_id);
        inner.fullscreen_owners.insert(space_id, owner_wid);
    }

    pub(crate) fn set_associated_windows(&self, wid: WinID, group: Vec<WinID>) {
        self.inner
            .force_write()
            .associated_windows
            .insert(wid, group);
    }

    pub(crate) fn set_defer_desktop_ops(&self, defer: bool) {
        self.inner.force_write().defer_desktop_ops = defer;
    }

    pub(crate) fn set_screen_locked(&self, locked: bool) {
        self.inner.force_write().screen_locked = locked;
    }

    pub(crate) fn flush_deferred_desktop_ops(&self) {
        let ops = std::mem::take(&mut self.inner.force_write().deferred_desktop_ops);
        for op in ops {
            op(self);
        }
    }

    fn run_or_defer_desktop(&self, op: impl FnOnce(&Self) + Send + Sync + 'static) {
        let defer = self.inner.force_read().defer_desktop_ops;
        if defer {
            self.inner
                .force_write()
                .deferred_desktop_ops
                .push(Box::new(op));
        } else {
            op(self);
        }
    }
}

impl DesktopBridgeApi for MockDesktopBridge {
    fn read_census(&self) -> Result<SpaceCensus, SpaceError> {
        let inner = self.0.inner.force_read();
        let mut display_ids: Vec<u32> = inner.displays.keys().copied().collect();
        display_ids.sort_unstable();
        let raw_displays = display_ids
            .into_iter()
            .filter_map(|id| {
                let d = inner.displays.get(&id)?;
                let current_space = SpaceId::new(d.current_workspace)?;
                let skylight_display = if id == super::TEST_DISPLAY_ID {
                    "Main".to_string()
                } else {
                    format!("display-{id}")
                };
                let spaces = d
                    .workspaces
                    .iter()
                    .filter_map(|&ws| {
                        let sid = SpaceId::new(ws)?;
                        let kind = if inner.fullscreen_spaces.contains(&ws) {
                            let owner = inner.fullscreen_owners.get(&ws).copied();
                            SpaceKind::Fullscreen {
                                owner_wid: owner,
                                single_tile: owner.is_some(),
                            }
                        } else {
                            SpaceKind::Desktop
                        };
                        Some((sid, format!("uuid-{ws}"), kind))
                    })
                    .collect();
                Some(RawDisplaySpaces {
                    display_id: id,
                    skylight_display,
                    current_space,
                    spaces,
                })
            })
            .collect();
        SpaceCensus::from_raw(raw_displays)
    }

    fn session_unlocked(&self) -> Result<(), SpaceError> {
        if self.0.inner.force_read().screen_locked {
            Err(SpaceError::new(
                SpaceErrorKind::SessionLocked,
                "mock screen locked",
            ))
        } else {
            Ok(())
        }
    }

    fn spaces_for_window(&self, window_id: WinID) -> Result<Vec<SpaceId>, SpaceError> {
        let inner = self.0.inner.force_read();
        let Some(win) = inner.windows.get(&window_id) else {
            return Ok(Vec::new());
        };
        let mut spaces: Vec<SpaceId> = std::iter::once(win.workspace_id)
            .chain(win.extra_workspaces.iter().copied())
            .filter_map(SpaceId::new)
            .collect();
        spaces.sort_unstable();
        spaces.dedup();
        Ok(spaces)
    }

    fn windows_in_space(
        &self,
        space_id: SpaceId,
        _include_minimized: bool,
    ) -> Result<Vec<WinID>, SpaceError> {
        let raw = space_id.get();
        let inner = self.0.inner.force_read();
        let mut ids: Vec<WinID> = inner
            .windows
            .values()
            .filter(|w| w.workspace_id == raw || w.extra_workspaces.contains(&raw))
            .map(|w| w.id)
            .collect();
        ids.sort_unstable();
        Ok(ids)
    }

    fn window_is_sticky(&self, window_id: WinID) -> Option<bool> {
        self.0
            .inner
            .force_read()
            .windows
            .get(&window_id)
            .map(|w| w.sticky)
    }

    fn associated_windows(&self, window_id: WinID) -> Result<Vec<WinID>, SpaceError> {
        let inner = self.0.inner.force_read();
        Ok(inner
            .associated_windows
            .get(&window_id)
            .cloned()
            .unwrap_or_else(|| vec![window_id]))
    }

    fn window_meta(&self, window_id: WinID) -> Option<WindowServerMeta> {
        self.0
            .inner
            .force_read()
            .windows
            .get(&window_id)
            .map(|w| WindowServerMeta {
                layer: 0,
                pid: w.pid,
            })
    }

    fn windows_for_pid(&self, pid: Pid) -> Vec<WinID> {
        let inner = self.0.inner.force_read();
        let mut ids: Vec<WinID> = inner
            .windows
            .values()
            .filter(|w| w.pid == pid && w.workspace_id != 0)
            .map(|w| w.id)
            .collect();
        ids.sort_unstable();
        ids
    }

    fn activate_space(
        &mut self,
        _skylight_display: &str,
        target: SpaceId,
        _hide: &[SpaceId],
    ) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let raw = target.get();
        self.0.run_or_defer_desktop(move |state| {
            let mut inner = state.inner.force_write();
            let found = inner
                .displays
                .values_mut()
                .find(|d| d.workspaces.contains(&raw));
            if let Some(display) = found {
                display.current_workspace = raw;
                inner
                    .event_queue
                    .push_back(Event::SpaceCurrentChanged { space_id: raw });
            }
        });
        Ok(())
    }

    fn create_desktop(&mut self) -> Result<SpaceId, SpaceError> {
        self.session_unlocked()?;
        let raw = {
            let mut inner = self.0.inner.force_write();
            let id = inner.next_space_id;
            inner.next_space_id += 1;
            id
        };
        let space_id = SpaceId::new(raw).expect("nonzero");
        self.0.run_or_defer_desktop(move |state| {
            let mut inner = state.inner.force_write();
            let active_disp = inner.active_display_id;
            if let Some(disp) = inner.displays.get_mut(&active_disp) {
                disp.workspaces.push(raw);
                inner
                    .event_queue
                    .push_back(Event::SpaceCreated { space_id: raw });
            }
        });
        Ok(space_id)
    }

    fn destroy_space(&mut self, space_id: SpaceId) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let raw = space_id.get();
        self.0.run_or_defer_desktop(move |state| {
            let mut inner = state.inner.force_write();
            let Some(active_ws) = inner
                .displays
                .values_mut()
                .find(|d| d.workspaces.contains(&raw) && d.current_workspace != raw)
                .map(|d| {
                    d.workspaces.retain(|&ws| ws != raw);
                    d.current_workspace
                })
            else {
                return;
            };
            for win in inner.windows.values_mut() {
                win.extra_workspaces.remove(&raw);
                if win.workspace_id == raw {
                    win.workspace_id = win
                        .extra_workspaces
                        .iter()
                        .next()
                        .copied()
                        .unwrap_or(active_ws);
                    win.extra_workspaces.remove(&win.workspace_id);
                }
            }
            inner
                .event_queue
                .push_back(Event::SpaceDestroyed { space_id: raw });
        });
        Ok(())
    }

    fn move_windows_to_space(
        &mut self,
        windows: &[WinID],
        space_id: SpaceId,
    ) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let raw = space_id.get();
        let win_vec = windows.to_vec();
        self.0.run_or_defer_desktop(move |state| {
            let mut inner = state.inner.force_write();
            for wid in win_vec {
                if let Some(win) = inner.windows.get_mut(&wid)
                    && !win.sticky
                {
                    win.workspace_id = raw;
                    win.extra_workspaces.clear();
                }
            }
            inner.event_queue.push_back(Event::SpaceMembershipChanged);
        });
        Ok(())
    }

    fn add_windows_to_space(
        &mut self,
        windows: &[WinID],
        space_id: SpaceId,
        remove_others: bool,
    ) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let raw = space_id.get();
        let win_vec = windows.to_vec();
        self.0.run_or_defer_desktop(move |state| {
            let mut inner = state.inner.force_write();
            for wid in win_vec {
                if let Some(win) = inner.windows.get_mut(&wid)
                    && !win.sticky
                {
                    if remove_others {
                        win.workspace_id = raw;
                        win.extra_workspaces.clear();
                    } else if win.workspace_id != raw {
                        win.extra_workspaces.insert(raw);
                    }
                }
            }
            inner.event_queue.push_back(Event::SpaceMembershipChanged);
        });
        Ok(())
    }

    fn assign_process(&mut self, pid: Pid, mode: AssignMode) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        self.0.run_or_defer_desktop(move |state| {
            let mut inner = state.inner.force_write();
            let display_desktops: Vec<(u32, WorkspaceId, Vec<WorkspaceId>)> = inner
                .displays
                .values()
                .map(|d| {
                    let desktops = d
                        .workspaces
                        .iter()
                        .copied()
                        .filter(|ws| !inner.fullscreen_spaces.contains(ws))
                        .collect();
                    (d.id, d.current_workspace, desktops)
                })
                .collect();
            for win in inner.windows.values_mut().filter(|w| w.pid == pid) {
                let owner_disp = display_desktops
                    .iter()
                    .find(|(_, _, list)| list.contains(&win.workspace_id))
                    .or_else(|| display_desktops.first());
                let Some((_, current_ws, desktops)) = owner_disp else {
                    continue;
                };
                match mode {
                    AssignMode::AllSpaces => {
                        win.sticky = true;
                        for &ws in desktops {
                            if ws != win.workspace_id {
                                win.extra_workspaces.insert(ws);
                            }
                        }
                    }
                    AssignMode::Space(target) => {
                        win.sticky = false;
                        win.workspace_id = target.get();
                        win.extra_workspaces.clear();
                    }
                    AssignMode::Clear => {
                        if win.sticky {
                            win.sticky = false;
                            win.workspace_id = *current_ws;
                            win.extra_workspaces.clear();
                        }
                    }
                }
            }
            inner.event_queue.push_back(Event::SpaceMembershipChanged);
        });
        Ok(())
    }

    fn move_space_to_index(
        &mut self,
        space_id: SpaceId,
        skylight_display: &str,
        index: u32,
    ) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let raw = space_id.get();
        let disp_name = skylight_display.to_string();
        self.0.run_or_defer_desktop(move |state| {
            let mut inner = state.inner.force_write();
            let target_id = if disp_name == "Main" {
                Some(super::TEST_DISPLAY_ID)
            } else {
                disp_name
                    .strip_prefix("display-")
                    .and_then(|s| s.parse::<u32>().ok())
            };
            if let Some(id) = target_id
                && let Some(disp) = inner.displays.get_mut(&id)
                && let Some(from) = disp.workspaces.iter().position(|&ws| ws == raw)
            {
                disp.workspaces.remove(from);
                let to = (index as usize).min(disp.workspaces.len());
                disp.workspaces.insert(to, raw);
            }
        });
        Ok(())
    }
}

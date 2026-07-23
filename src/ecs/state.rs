use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bevy::app::AppExit;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::Has;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::Query;
use bevy::math::IRect;
use objc2_core_graphics::CGDirectDisplayID;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::ecs::layout::{Column, LayoutStrip, StackItem};
use crate::ecs::params::Windows;
use crate::ecs::{ActiveDisplayMarker, ActiveWorkspaceMarker, SelectedVirtualMarker, Unmanaged};
use crate::manager::{Application, Display, WindowManager};
use crate::platform::{Pid, ProcessSerialNumber, WinID, WorkspaceId};

pub const STATE_FILE_NAME: &str = "state.json";
const SUPPORTED_STATE_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Resource)]
pub struct PaneruState {
    pub version: u32,
    pub timestamp: u64,
    pub active_display_id: Option<CGDirectDisplayID>,
    #[serde(default)]
    pub displays: Vec<SavedDisplay>,
    pub workspaces: Vec<SavedWorkspace>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SavedDisplay {
    pub display_id: CGDirectDisplayID,
    pub bounds: SavedRect,
    pub active: bool,
    pub workspace_ids: Vec<WorkspaceId>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedRect {
    pub min_x: i32,
    pub min_y: i32,
    pub max_x: i32,
    pub max_y: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SavedWorkspace {
    pub workspace_id: WorkspaceId,
    pub display_id: Option<CGDirectDisplayID>,
    pub active_virtual_index: Option<u32>,
    pub strips: Vec<SavedStrip>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SavedStrip {
    pub virtual_index: u32,
    pub columns: Vec<SavedColumn>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum SavedColumn {
    Single(SavedWindow),
    Stack(Vec<SavedStackItem>),
    Tabs(Vec<SavedWindow>),
    Fullscreen(SavedWindow),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum SavedStackItem {
    Single(SavedWindow),
    Tabs(Vec<SavedWindow>),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SavedWindow {
    // Primary matching (stable across WM restarts)
    pub window_id: WinID,
    pub pid: Pid,
    pub psn: ProcessSerialNumber,

    // Heuristic matching (if IDs change or apps restarted)
    pub bundle_id: String,
    pub title: String,
    pub identifier: String,
    pub role: String,
    pub subrole: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateQueryKind {
    State,
    VirtualWorkspaces,
    Active,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PaneruQueryState {
    pub version: u32,
    pub timestamp: u64,
    pub active: PaneruActiveState,
    pub virtual_workspaces: Vec<PaneruVirtualWorkspaceState>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PaneruActiveState {
    pub display_id: Option<CGDirectDisplayID>,
    pub native_workspace_id: Option<WorkspaceId>,
    pub virtual_workspace_number: Option<u32>,
    pub focused_window_id: Option<WinID>,
    pub focused_bundle_id: Option<String>,
    pub focused_app_name: Option<String>,
    pub focused_window_title: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PaneruVirtualWorkspaceState {
    pub number: u32,
    pub native_workspace_id: WorkspaceId,
    pub active: bool,
    pub windows: Vec<PaneruWindowState>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PaneruWindowState {
    pub window_id: WinID,
    pub bundle_id: String,
    pub app_name: String,
    pub title: String,
    pub focused: bool,
    pub floating: bool,
}

impl From<IRect> for SavedRect {
    fn from(rect: IRect) -> Self {
        Self {
            min_x: rect.min.x,
            min_y: rect.min.y,
            max_x: rect.max.x,
            max_y: rect.max.y,
        }
    }
}

impl SavedWindow {
    pub fn from_entity(
        entity: Entity,
        windows: &Windows,
        apps: &Query<&Application>,
    ) -> Option<Self> {
        let window = windows.get(entity)?;
        let (_, _, app_entity) = windows.find_parent(window.id())?;
        let app = apps.get(app_entity).ok()?;

        Some(Self {
            window_id: window.id(),
            pid: window.pid().ok()?,
            psn: app.psn(),
            bundle_id: app.bundle_id().unwrap_or_default().clone(),
            title: window.title().unwrap_or_default(),
            identifier: window.identifier().unwrap_or_default(),
            role: window.role().unwrap_or_default(),
            subrole: window.subrole().unwrap_or_default(),
        })
    }

    pub fn hard_match(&self, other_id: WinID, other_proc_id: Pid, other_bundle: &str) -> bool {
        // 1. Exact match (including bundle to avoid cross-app PID collisions in edge cases)
        self.window_id == other_id && self.pid == other_proc_id && self.bundle_id == other_bundle
    }
}

impl PaneruState {
    #[allow(clippy::type_complexity, clippy::too_many_lines)]
    pub fn extract(
        workspaces: &Query<(Option<&ChildOf>, &LayoutStrip, Has<ActiveWorkspaceMarker>)>,
        displays: &Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
        windows: &Windows,
        apps: &Query<&Application>,
    ) -> Self {
        let mut display_entity_ids = HashMap::new();
        let mut display_workspace_ids: HashMap<Entity, Vec<WorkspaceId>> = HashMap::new();
        let mut workspace_map: HashMap<WorkspaceId, SavedWorkspaceBuilder> = HashMap::new();
        let active_display_id = displays
            .iter()
            .find(|(_, _, active)| *active)
            .map(|(display, _, _)| display.id());

        for (display, entity, _) in displays {
            display_entity_ids.insert(entity, display.id());
            display_workspace_ids.insert(entity, Vec::new());
        }

        for (child, strip, active_workspace) in workspaces {
            let display_entity = child.map(ChildOf::parent);
            let display_id =
                display_entity.and_then(|entity| display_entity_ids.get(&entity).copied());
            if let Some(entity) = display_entity
                && let Some(workspace_ids) = display_workspace_ids.get_mut(&entity)
                && !workspace_ids.contains(&strip.id())
            {
                workspace_ids.push(strip.id());
            }

            let mut saved_columns = Vec::new();
            for col in strip.columns() {
                let saved_col = match col {
                    Column::Single(entity) => {
                        SavedWindow::from_entity(*entity, windows, apps).map(SavedColumn::Single)
                    }
                    Column::Stack(items) => {
                        let saved_items = items
                            .iter()
                            .filter_map(|item| match item {
                                StackItem::Single(entity) => {
                                    SavedWindow::from_entity(*entity, windows, apps)
                                        .map(SavedStackItem::Single)
                                }
                                StackItem::Tabs(tabs) => {
                                    let saved_tabs: Vec<_> = tabs
                                        .iter()
                                        .filter_map(|&e| SavedWindow::from_entity(e, windows, apps))
                                        .collect();
                                    if saved_tabs.is_empty() {
                                        None
                                    } else {
                                        Some(SavedStackItem::Tabs(saved_tabs))
                                    }
                                }
                            })
                            .collect::<Vec<_>>();
                        if saved_items.is_empty() {
                            None
                        } else {
                            Some(SavedColumn::Stack(saved_items))
                        }
                    }
                    Column::Tabs(tabs) => {
                        let saved_tabs: Vec<_> = tabs
                            .iter()
                            .filter_map(|&e| SavedWindow::from_entity(e, windows, apps))
                            .collect();
                        if saved_tabs.is_empty() {
                            None
                        } else {
                            Some(SavedColumn::Tabs(saved_tabs))
                        }
                    }
                    Column::Fullscren(entity) => SavedWindow::from_entity(*entity, windows, apps)
                        .map(SavedColumn::Fullscreen),
                };

                if let Some(sc) = saved_col {
                    saved_columns.push(sc);
                }
            }

            let workspace =
                workspace_map
                    .entry(strip.id())
                    .or_insert_with(|| SavedWorkspaceBuilder {
                        display_id,
                        active_virtual_index: None,
                        strips: Vec::new(),
                    });
            if workspace.display_id.is_none() {
                workspace.display_id = display_id;
            }
            if active_workspace {
                workspace.active_virtual_index = Some(strip.virtual_index);
            }
            workspace.strips.push(SavedStrip {
                virtual_index: strip.virtual_index,
                columns: saved_columns,
            });
        }

        let workspaces = workspace_map
            .into_iter()
            .map(|(workspace_id, mut workspace)| {
                workspace.strips.sort_by_key(|s| s.virtual_index);
                SavedWorkspace {
                    workspace_id,
                    display_id: workspace.display_id,
                    active_virtual_index: workspace.active_virtual_index,
                    strips: workspace.strips,
                }
            })
            .collect();
        let displays = displays
            .iter()
            .map(|(display, entity, active)| SavedDisplay {
                display_id: display.id(),
                bounds: display.bounds().into(),
                active,
                workspace_ids: display_workspace_ids.remove(&entity).unwrap_or_default(),
            })
            .collect();

        Self {
            version: SUPPORTED_STATE_VERSION,
            timestamp: now_timestamp(),
            active_display_id,
            displays,
            workspaces,
        }
    }

    pub fn save_to_file(&self, path: &Path) -> Result<(), std::io::Error> {
        let json = serde_json::to_string_pretty(self).map_err(|e| {
            error!("Failed to serialize state: {e}");
            std::io::Error::other(e)
        })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, json)?;
        fs::rename(tmp_path, path)?;
        Ok(())
    }

    pub fn load_from_file(path: &Path) -> Option<Self> {
        let data = fs::read_to_string(path).ok()?;
        let state: Self = serde_json::from_str(&data).ok()?;
        (state.version == SUPPORTED_STATE_VERSION).then_some(state)
    }

    pub fn default_state_file_path() -> PathBuf {
        xdg::BaseDirectories::with_prefix("paneru")
            .get_state_file(STATE_FILE_NAME)
            .expect("XDG state directory should be available")
    }

    #[cfg(test)]
    pub fn find_match(
        &self,
        window_id: WinID,
        pid: Pid,
        bundle_id: &str,
    ) -> Option<(WorkspaceId, u32, usize, SavedWindow)> {
        for workspace in &self.workspaces {
            for strip in &workspace.strips {
                for (col_idx, column) in strip.columns.iter().enumerate() {
                    let match_in_col = |sw: &SavedWindow| {
                        if sw.hard_match(window_id, pid, bundle_id) {
                            return Some(sw.clone());
                        }
                        None
                    };

                    match column {
                        SavedColumn::Single(sw) | SavedColumn::Fullscreen(sw) => {
                            if let Some(matched) = match_in_col(sw) {
                                return Some((
                                    workspace.workspace_id,
                                    strip.virtual_index,
                                    col_idx,
                                    matched,
                                ));
                            }
                        }
                        SavedColumn::Stack(items) => {
                            for item in items {
                                match item {
                                    SavedStackItem::Single(sw) => {
                                        if let Some(matched) = match_in_col(sw) {
                                            return Some((
                                                workspace.workspace_id,
                                                strip.virtual_index,
                                                col_idx,
                                                matched,
                                            ));
                                        }
                                    }
                                    SavedStackItem::Tabs(tabs) => {
                                        for sw in tabs {
                                            if let Some(matched) = match_in_col(sw) {
                                                return Some((
                                                    workspace.workspace_id,
                                                    strip.virtual_index,
                                                    col_idx,
                                                    matched,
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        SavedColumn::Tabs(tabs) => {
                            for sw in tabs {
                                if let Some(matched) = match_in_col(sw) {
                                    return Some((
                                        workspace.workspace_id,
                                        strip.virtual_index,
                                        col_idx,
                                        matched,
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
        None
    }
}

#[derive(Default)]
struct SavedWorkspaceBuilder {
    display_id: Option<CGDirectDisplayID>,
    active_virtual_index: Option<u32>,
    strips: Vec<SavedStrip>,
}

impl PaneruQueryState {
    #[allow(clippy::type_complexity)]
    pub fn extract(
        workspaces: &Query<(
            &ChildOf,
            &LayoutStrip,
            Has<ActiveWorkspaceMarker>,
            Has<SelectedVirtualMarker>,
        )>,
        displays: &Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
        windows: &Windows,
        apps: &Query<&Application>,
        window_manager: &WindowManager,
    ) -> Self {
        let focused_entity = windows.focused().map(|(_, entity)| entity);

        let active_display = displays
            .iter()
            .find_map(|(display, entity, active)| active.then_some((display.id(), entity)));

        let mut virtual_workspaces = Vec::new();
        let mut workspace_max_numbers: HashMap<WorkspaceId, u32> = HashMap::new();
        let mut active = PaneruActiveState {
            display_id: active_display.map(|(display_id, _)| display_id),
            ..PaneruActiveState::default()
        };

        for (child, strip, active_workspace, selected_workspace) in workspaces {
            let floating = (active_workspace || selected_workspace)
                .then(|| {
                    window_manager
                        .windows_in_workspace(strip.id())
                        .unwrap_or_default()
                })
                .into_iter()
                .flatten()
                .filter_map(|window_id| {
                    let (_, entity) = windows.find(window_id)?;
                    let (_, _, unmanaged) = windows.get_managed(entity)?;
                    (matches!(unmanaged, Some(Unmanaged::Floating)) && !strip.contains(entity))
                        .then_some(entity)
                });
            let row_windows = strip
                .all_windows()
                .into_iter()
                .chain(floating)
                .filter_map(|entity| {
                    let (window, _, unmanaged) = windows.get_managed(entity)?;
                    let (_, _, app_entity) = windows.find_parent(window.id())?;
                    let app = apps.get(app_entity).ok()?;
                    let bundle_id = app.bundle_id().unwrap_or_default().clone();
                    let app_name = app.name().to_string();
                    let title = window.title().unwrap_or_default();
                    Some(PaneruWindowState {
                        window_id: window.id(),
                        bundle_id,
                        app_name,
                        title,
                        focused: focused_entity == Some(entity),
                        floating: matches!(unmanaged, Some(Unmanaged::Floating)),
                    })
                })
                .collect::<Vec<_>>();

            let number = strip.virtual_index + 1;
            workspace_max_numbers
                .entry(strip.id())
                .and_modify(|max| *max = (*max).max(number))
                .or_insert(number);
            if active_workspace {
                active.native_workspace_id = Some(strip.id());
                active.virtual_workspace_number = Some(number);
            }

            if active_workspace
                && let Some(window) = row_windows.iter().find(|window| window.focused)
            {
                active.focused_window_id = Some(window.window_id);
                active.focused_bundle_id = Some(window.bundle_id.clone());
                active.focused_app_name = Some(window.app_name.clone());
                active.focused_window_title = Some(window.title.clone());
            }

            virtual_workspaces.push(PaneruVirtualWorkspaceState {
                number,
                native_workspace_id: strip.id(),
                active: active_workspace,
                windows: row_windows,
            });

            if active_workspace
                && let Some((display_id, display_entity)) = active_display
                && child.parent() == display_entity
            {
                active.display_id = Some(display_id);
            }
        }

        let present_numbers = virtual_workspaces
            .iter()
            .map(|workspace| (workspace.native_workspace_id, workspace.number))
            .collect::<HashSet<_>>();
        for (workspace_id, max_number) in workspace_max_numbers {
            for number in 1..=max_number {
                if !present_numbers.contains(&(workspace_id, number)) {
                    virtual_workspaces.push(PaneruVirtualWorkspaceState {
                        number,
                        native_workspace_id: workspace_id,
                        active: false,
                        windows: Vec::new(),
                    });
                }
            }
        }

        virtual_workspaces
            .sort_by_key(|workspace| (workspace.native_workspace_id, workspace.number));

        Self {
            version: 1,
            timestamp: now_timestamp(),
            active,
            virtual_workspaces,
        }
    }

    pub fn to_query_json(&self, kind: StateQueryKind) -> serde_json::Result<String> {
        match kind {
            StateQueryKind::State => serde_json::to_string(self),
            StateQueryKind::VirtualWorkspaces => serde_json::to_string(&self.virtual_workspaces),
            StateQueryKind::Active => serde_json::to_string(&self.active),
        }
    }
}

fn now_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[allow(clippy::needless_pass_by_value)]
pub fn periodic_state_save(
    workspaces: Query<(Option<&ChildOf>, &LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    displays: Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
    windows: Windows,
    apps: Query<&Application>,
) {
    let state = PaneruState::extract(&workspaces, &displays, &windows, &apps);
    let path = PaneruState::default_state_file_path();
    if let Err(e) = state.save_to_file(&path) {
        warn!("Failed to save state: {e}");
    } else {
        debug!("State saved to {}", path.display());
    }
}

#[allow(clippy::needless_pass_by_value)]
pub fn cleanup_on_exit(
    mut exit_events: MessageReader<AppExit>,
    workspaces: Query<(Option<&ChildOf>, &LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    displays: Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
    windows: Windows,
    apps: Query<&Application>,
) {
    if exit_events.read().next().is_some() {
        info!("Exiting, saving state...");
        let state = PaneruState::extract(&workspaces, &displays, &windows, &apps);
        let path = PaneruState::default_state_file_path();
        if let Err(e) = state.save_to_file(&path) {
            error!("Failed to save state on exit: {e}");
        }
    }
}

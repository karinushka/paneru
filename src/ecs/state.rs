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
use bevy::ecs::system::{Query, Res, SystemParam};
use bevy::math::IRect;
use objc2_core_graphics::CGDirectDisplayID;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::ecs::layout::{Column, LayoutStrip, StackItem};
use crate::ecs::params::Windows;
use crate::ecs::{ActiveDisplayMarker, ActiveWorkspaceMarker, SelectedVirtualMarker, Unmanaged};
use crate::manager::{Application, Display, WindowManager};
use crate::platform::{Pid, ProcessSerialNumber, WinID, WorkspaceId};
use paneru_shared_types::windowset::WindowSet;

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

// These wire-format types live in the shared `paneru_shared_types` crate;
// aliased here to the names the rest of the daemon already uses.
pub use paneru_shared_types::state::{
    ActiveState as PaneruActiveState, Frame, QueryState as PaneruQueryState, StateEvent,
    StateQueryKind, VirtualWorkspaceState as PaneruVirtualWorkspaceState,
    WindowState as PaneruWindowState,
};

/// Resolves which display a window frame is on and whether more than a sliver of
/// it is showing there. Returns `None` when the frame misses every display.
fn window_visibility(
    frame: IRect,
    displays: &Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
    sliver_width: i32,
) -> Option<(CGDirectDisplayID, bool)> {
    displays
        .iter()
        // Pick the display showing the largest slice of the window.
        .map(|(display, _, _)| {
            let overlap = frame.intersect(display.bounds());
            let (width, height) = (overlap.width().max(0), overlap.height().max(0));
            (display.id(), width, height)
        })
        .filter(|(_, width, height)| *width > 0 && *height > 0)
        .max_by_key(|(_, width, height)| i64::from(*width) * i64::from(*height))
        .map(|(display_id, width, height)| (display_id, width > sliver_width && height > 0))
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
    #[allow(clippy::too_many_lines)]
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

/// The world access [`QueryState::extract`] needs, bundled so callers (the
/// socket query handler, the embedded Lua runtime) take one parameter instead
/// of six.
#[derive(SystemParam)]
pub struct QueryStateParams<'w, 's> {
    workspaces: Query<
        'w,
        's,
        (
            &'static ChildOf,
            &'static LayoutStrip,
            Has<ActiveWorkspaceMarker>,
            Has<SelectedVirtualMarker>,
        ),
    >,
    displays: Query<'w, 's, (&'static Display, Entity, Has<ActiveDisplayMarker>)>,
    windows: Windows<'w, 's>,
    apps: Query<'w, 's, &'static Application>,
    window_manager: Res<'w, WindowManager>,
    config: Res<'w, Config>,
}

impl QueryStateParams<'_, '_> {
    /// The window queries backing the extract, for callers that also need to
    /// look a window up directly rather than through the state document.
    pub fn windows(&self) -> &Windows<'_, '_> {
        &self.windows
    }

    /// Builds the state document from the current world.
    ///
    /// # Errors
    ///
    /// Returns an error if the window manager cannot enumerate a workspace.
    pub fn extract(&self) -> crate::errors::Result<PaneruQueryState> {
        PaneruQueryState::extract(
            &self.workspaces,
            &self.displays,
            &self.windows,
            &self.apps,
            &self.window_manager,
            &self.config,
        )
    }
}

/// Reads the layout as the tree a script transforms. Unlike
/// [`QueryState::extract`], which flattens each workspace into a list of
/// windows, this keeps the strip's column structure — needed for `ws:swap`,
/// `ws:east`, `ws:stack` and friends to know what is beside what.
impl QueryStateParams<'_, '_> {
    pub fn extract_window_set(&self) -> crate::errors::Result<WindowSet> {
        use paneru_shared_types::windowset::{ColumnSet, DisplaySet, WorkspaceSet};

        let focused_entity = self.windows.focused().map(|(_, entity)| entity);
        let sliver_width = self.config.sliver_width();
        let active_workspace_id = self
            .workspaces
            .iter()
            .find_map(|(_, strip, active, _)| active.then_some(strip.id()));

        // Group the workspace strips by the display entity that owns them, so
        // each display can be built with its own workspaces in one pass.
        let mut strips_by_display: HashMap<Entity, Vec<WorkspaceSet>> = HashMap::new();
        for (child, strip, active_workspace, selected_workspace) in self.workspaces {
            // Only ask for floating windows on a workspace that's actually
            // showing, since this read goes out to the window server.
            let floating_entities = if active_workspace
                || selected_workspace && active_workspace_id != Some(strip.id())
            {
                self.window_manager.windows_in_workspace(strip.id())?
            } else {
                Vec::new()
            };

            // A window stays tracked by the strip after it's floated (so it can
            // be re-tiled later), so it's the `Floating` marker — not strip
            // membership — that decides whether it goes in `columns` or `floating`.
            let mut floating = Vec::new();
            let mut columns: Vec<ColumnSet> = Vec::new();
            for column in strip.columns() {
                let mut tiled = Vec::new();
                for entity in column.window_iter() {
                    let Some(record) = self.window_record(entity, focused_entity, sliver_width)
                    else {
                        continue;
                    };
                    if record.floating {
                        floating.push(record);
                    } else {
                        tiled.push(record);
                    }
                }
                if tiled.is_empty() {
                    continue;
                }
                let width_ratio = column
                    .window_iter()
                    .find_map(|entity| self.windows.width_ratio(entity))
                    .unwrap_or(1.0);
                let selected = column
                    .top()
                    .and_then(|top| self.windows.get(top).map(|window| window.id()))
                    .and_then(|id| tiled.iter().position(|window| window.id == id))
                    .unwrap_or(0);
                columns.push(ColumnSet {
                    kind: column_kind(column),
                    width_ratio,
                    selected,
                    windows: std::sync::Arc::new(tiled),
                });
            }

            // Floating windows the strip never knew about.
            floating.extend(
                floating_entities
                    .into_iter()
                    .filter_map(|window_id| {
                        let (_, entity) = self.windows.find(window_id)?;
                        let (_, _, unmanaged) = self.windows.get_managed(entity)?;
                        (matches!(unmanaged, Some(Unmanaged::Floating)) && !strip.contains(entity))
                            .then_some(entity)
                    })
                    .filter_map(|entity| self.window_record(entity, focused_entity, sliver_width)),
            );

            strips_by_display
                .entry(child.parent())
                .or_default()
                .push(WorkspaceSet {
                    number: strip.virtual_index + 1,
                    native_id: strip.id(),
                    active: active_workspace,
                    columns: std::sync::Arc::new(columns),
                    floating: std::sync::Arc::new(floating),
                });
        }

        let displays = self
            .displays
            .iter()
            .map(|(display, entity, active)| {
                let bounds = display.bounds();
                let mut workspaces = strips_by_display.remove(&entity).unwrap_or_default();
                workspaces.sort_by_key(|workspace| workspace.number);
                DisplaySet {
                    id: display.id(),
                    frame: Frame {
                        x: bounds.min.x,
                        y: bounds.min.y,
                        width: bounds.width(),
                        height: bounds.height(),
                    },
                    active,
                    workspaces: std::sync::Arc::new(workspaces),
                }
            })
            .collect();

        let focused = focused_entity
            .and_then(|entity| self.windows.get(entity))
            .map(|window| window.id());
        Ok(WindowSet::new(displays, focused))
    }

    /// One window, as a script sees it. `None` for an entity that is no longer
    /// a window we know anything about.
    fn window_record(
        &self,
        entity: Entity,
        focused: Option<Entity>,
        sliver_width: i32,
    ) -> Option<paneru_shared_types::windowset::WindowRec> {
        let (window, _, unmanaged) = self.windows.get_managed(entity)?;
        let (_, _, app_entity) = self.windows.find_parent(window.id())?;
        let app = self.apps.get(app_entity).ok()?;
        let frame = self.windows.frame(entity);
        // Minimized and hidden windows are never on screen, whatever their last
        // known frame says.
        let hidden = matches!(unmanaged, Some(Unmanaged::Minimized | Unmanaged::Hidden));
        let visible = frame
            .and_then(|frame| window_visibility(frame, &self.displays, sliver_width))
            .is_some_and(|(_, visible)| visible && !hidden);

        Some(paneru_shared_types::windowset::WindowRec {
            id: window.id(),
            app_name: app.name().to_string(),
            bundle_id: app.bundle_id().unwrap_or_default().clone(),
            title: window.title().unwrap_or_default(),
            frame: frame.map(|frame| Frame {
                x: frame.min.x,
                y: frame.min.y,
                width: frame.width(),
                height: frame.height(),
            }),
            floating: matches!(unmanaged, Some(Unmanaged::Floating)),
            managed: unmanaged.is_none(),
            visible,
            focused: focused == Some(entity),
        })
    }
}

/// How a layout column arranges its windows, in the vocabulary a script sees.
fn column_kind(column: &Column) -> paneru_shared_types::windowset::ColumnKind {
    use paneru_shared_types::windowset::ColumnKind;
    match column {
        Column::Single(_) => ColumnKind::Single,
        Column::Stack(_) => ColumnKind::Stack,
        Column::Tabs(_) => ColumnKind::Tabs,
        Column::Fullscren(_) => ColumnKind::Fullscreen,
    }
}

pub trait QueryState: std::marker::Sized {
    fn extract(
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
        config: &Config,
    ) -> crate::errors::Result<Self>;
}

/// Builds the query/subscribe state document from the ECS world.
///
/// A free function rather than an inherent method because [`PaneruQueryState`]
/// belongs to the shared protocol crate, which knows nothing about the ECS.
impl QueryState for PaneruQueryState {
    #[allow(clippy::too_many_lines)]
    fn extract(
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
        config: &Config,
    ) -> crate::errors::Result<Self> {
        let focused_entity = windows.focused().map(|(_, entity)| entity);
        let sliver_width = config.sliver_width();

        let active_display = displays
            .iter()
            .find_map(|(display, entity, active)| active.then_some((display.id(), entity)));
        let active_workspace_id = workspaces
            .iter()
            .find_map(|(_, strip, active, _)| active.then_some(strip.id()));

        let mut virtual_workspaces = Vec::new();
        let mut workspace_max_numbers: HashMap<WorkspaceId, u32> = HashMap::new();
        let mut active = PaneruActiveState {
            display_id: active_display.map(|(display_id, _)| display_id),
            ..PaneruActiveState::default()
        };

        for (child, strip, active_workspace, selected_workspace) in workspaces {
            let floating = if active_workspace
                || selected_workspace && active_workspace_id != Some(strip.id())
            {
                window_manager.windows_in_workspace(strip.id())?
            } else {
                Vec::new()
            }
            .into_iter()
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
                    let frame = windows.frame(entity);
                    // Minimized and hidden windows are never on screen, whatever
                    // their last known frame says.
                    let hidden =
                        matches!(unmanaged, Some(Unmanaged::Minimized | Unmanaged::Hidden));
                    let visibility = frame
                        .and_then(|frame| window_visibility(frame, displays, sliver_width))
                        .map(|(display_id, visible)| (display_id, visible && !hidden));
                    Some(PaneruWindowState {
                        window_id: window.id(),
                        bundle_id,
                        app_name,
                        title,
                        focused: focused_entity == Some(entity),
                        floating: matches!(unmanaged, Some(Unmanaged::Floating)),
                        display_id: visibility.map(|(display_id, _)| display_id),
                        frame: frame.map(|frame| Frame {
                            x: frame.min.x,
                            y: frame.min.y,
                            width: frame.width(),
                            height: frame.height(),
                        }),
                        visible: visibility.is_some_and(|(_, visible)| visible),
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

        Ok(PaneruQueryState {
            version: 1,
            timestamp: now_timestamp(),
            active,
            virtual_workspaces,
        })
    }
}

fn now_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

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

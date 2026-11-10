use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use bevy::app::{App, Plugin, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Changed, Has, With};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::{not, resource_exists};
use bevy::ecs::system::{Commands, Populated, Query, Res, ResMut};
use bevy::prelude::Event as BevyEvent;
use bevy::time::Time;
use bevy::time::common_conditions::on_timer;
use objc2_core_graphics::CGDirectDisplayID;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::commands::Command;
use crate::config::Config;
use crate::ecs::display::DisplayReconcileState;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::Windows;
use crate::ecs::restore::SessionRestore;
use crate::ecs::state::{
    PaneruState, SavedColumn, SavedDisplay, SavedStackItem, SavedWindow, SavedWorkspace,
};
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, DockPosition, Initializing, RestoreWindowState,
    SendMessageTrigger,
};
use crate::events::Event;
use crate::manager::{Application, Display, WindowManager};
use crate::platform::{WinID, WorkspaceId};
use crate::types::windowset::LayoutOp;

const SETUPS_FILE_NAME: &str = "setups.json";
const CAPTURE_DEBOUNCE: Duration = Duration::from_secs(2);
const CAPTURE_CHECK: Duration = Duration::from_millis(500);

/// Fires after the final reconciliation pass, not the first.
#[derive(BevyEvent)]
pub(crate) struct DisplaysSettled;

#[derive(Default, Serialize, Deserialize)]
struct SetupsFile {
    setups: BTreeMap<String, PaneruState>,
}

#[derive(Default, Resource)]
pub(crate) struct DisplaySetups {
    // No path keeps the store in memory only.
    path: Option<PathBuf>,
    setups: BTreeMap<String, PaneruState>,
    // The setup whose layout is in place. A capture writes only under this key.
    settled: Option<String>,
    changed_at: Option<Duration>,
}

impl DisplaySetups {
    pub(crate) fn load() -> Self {
        let path = PaneruState::default_state_file_path().with_file_name(SETUPS_FILE_NAME);
        let setups = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| {
                serde_json::from_str::<SetupsFile>(&text)
                    .inspect_err(|err| warn!("Ignoring unreadable {}: {err}", path.display()))
                    .ok()
            })
            .map(|file| file.setups)
            .unwrap_or_default();
        Self {
            path: Some(path),
            setups,
            ..Self::default()
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let file = SetupsFile {
            setups: self.setups.clone(),
        };
        let written = serde_json::to_string_pretty(&file)
            .map_err(std::io::Error::other)
            .and_then(|json| {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let tmp_path = path.with_extension("json.tmp");
                std::fs::write(&tmp_path, json)?;
                std::fs::rename(tmp_path, path)
            });
        if let Err(err) = written {
            warn!("Failed to save display setups: {err}");
        }
    }

    #[cfg(test)]
    pub(crate) fn saved(&self, key: &str) -> Option<&PaneruState> {
        self.setups.get(key)
    }
}

pub struct DisplaySetupsPlugin;

impl Plugin for DisplaySetupsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DisplaySetups>()
            .add_systems(
                Update,
                (
                    note_layout_change,
                    capture_setup
                        .run_if(on_timer(CAPTURE_CHECK))
                        .run_if(not(resource_exists::<Initializing>))
                        .run_if(not(resource_exists::<SessionRestore>))
                        .run_if(|reconcile: Res<DisplayReconcileState>| reconcile.is_idle()),
                )
                    .chain()
                    .in_set(super::UpdateSet::Housekeeping)
                    .run_if(|config: Res<Config>| config.remember_display_setups()),
            )
            .add_observer(restore_setup);
    }
}

pub(crate) fn setup_key(window_manager: &WindowManager) -> Option<String> {
    let uuids = window_manager
        .present_displays()
        .iter()
        .map(|(display, _)| window_manager.display_uuid(display.id()))
        .collect::<Option<BTreeSet<_>>>()?;
    (!uuids.is_empty()).then(|| uuids.into_iter().collect::<Vec<_>>().join("+"))
}

fn note_layout_change(
    _changed: Populated<(), Changed<LayoutStrip>>,
    time: Res<Time>,
    mut setups: ResMut<DisplaySetups>,
) {
    setups.changed_at = Some(time.elapsed());
}

#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
fn capture_setup(
    workspaces: Query<(Option<&ChildOf>, &LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    displays: Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
    docks: Query<Option<&DockPosition>, With<Display>>,
    windows: Windows,
    apps: Query<&Application>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    time: Res<Time>,
    mut setups: ResMut<DisplaySetups>,
) {
    let Some(changed_at) = setups.changed_at else {
        return;
    };
    if time.elapsed().saturating_sub(changed_at) < CAPTURE_DEBOUNCE {
        return;
    }
    let Some(key) = setup_key(&window_manager) else {
        return;
    };
    // The first capture after start adopts the startup layout for this setup.
    let settled = setups.settled.get_or_insert_with(|| key.clone()).clone();
    if settled != key {
        return;
    }

    let ratios = width_ratios(&workspaces, &displays, &docks, &windows, &config);
    let present = window_manager.present_displays();
    let state = PaneruState::extract(&workspaces, &displays, &windows, &apps);
    let snapshot = PaneruState {
        displays: state
            .displays
            .iter()
            .map(|saved| describe_display(saved, &present, &window_manager))
            .collect(),
        workspaces: state
            .workspaces
            .iter()
            .map(|workspace| workspace_with_widths(workspace, &ratios))
            .collect(),
        ..state
    };
    debug!("captured the layout of display setup {key}");
    setups.changed_at = None;
    setups.setups.insert(key, snapshot);
    setups.save();
}

fn width_ratios(
    workspaces: &Query<(Option<&ChildOf>, &LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    displays: &Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
    docks: &Query<Option<&DockPosition>, With<Display>>,
    windows: &Windows,
    config: &Config,
) -> HashMap<WinID, f64> {
    workspaces
        .iter()
        .filter_map(|(parent, strip, _)| {
            let parent = parent?.parent();
            let (display, _, _) = displays.get(parent).ok()?;
            let dock = docks.get(parent).ok().flatten();
            let viewport = display.actual_display_bounds(dock, config).width();
            (viewport > 0).then_some((strip, f64::from(viewport)))
        })
        .flat_map(|(strip, viewport)| {
            strip.all_windows().into_iter().filter_map(move |entity| {
                let window = windows.get(entity)?;
                let size = windows.size(entity)?;
                Some((window.id(), f64::from(size.x) / viewport))
            })
        })
        .collect()
}

/// The remap needs the spaces in macOS order, which `PaneruState::extract` does not give.
fn describe_display(
    saved: &SavedDisplay,
    present: &[(Display, Vec<WorkspaceId>)],
    window_manager: &WindowManager,
) -> SavedDisplay {
    let workspace_ids = present
        .iter()
        .find(|(display, _)| display.id() == saved.display_id)
        .map_or_else(|| saved.workspace_ids.clone(), |(_, spaces)| spaces.clone());
    SavedDisplay {
        uuid: window_manager.display_uuid(saved.display_id),
        workspace_ids,
        ..saved.clone()
    }
}

fn workspace_with_widths(
    workspace: &SavedWorkspace,
    ratios: &HashMap<WinID, f64>,
) -> SavedWorkspace {
    let with_width = |window: &SavedWindow| SavedWindow {
        width_ratio: ratios.get(&window.window_id).copied(),
        ..window.clone()
    };
    let with_widths = |windows: &[SavedWindow]| windows.iter().map(with_width).collect();
    SavedWorkspace {
        strips: workspace
            .strips
            .iter()
            .map(|strip| crate::ecs::state::SavedStrip {
                columns: strip
                    .columns
                    .iter()
                    .map(|column| match column {
                        SavedColumn::Single(window) => SavedColumn::Single(with_width(window)),
                        SavedColumn::Fullscreen(window) => {
                            SavedColumn::Fullscreen(with_width(window))
                        }
                        SavedColumn::Tabs(windows) => SavedColumn::Tabs(with_widths(windows)),
                        SavedColumn::Stack(items) => SavedColumn::Stack(
                            items
                                .iter()
                                .map(|item| match item {
                                    SavedStackItem::Single(window) => {
                                        SavedStackItem::Single(with_width(window))
                                    }
                                    SavedStackItem::Tabs(windows) => {
                                        SavedStackItem::Tabs(with_widths(windows))
                                    }
                                })
                                .collect(),
                        ),
                    })
                    .collect(),
                ..strip.clone()
            })
            .collect(),
        ..workspace.clone()
    }
}

/// A saved space with no match on the same display UUID and position is dropped.
pub(crate) fn remap_to_present(
    snapshot: &PaneruState,
    present: &[(Display, Vec<WorkspaceId>)],
    uuid_of: impl Fn(CGDirectDisplayID) -> Option<String>,
) -> PaneruState {
    let present_by_uuid = present
        .iter()
        .filter_map(|(display, spaces)| Some((uuid_of(display.id())?, (display.id(), spaces))))
        .collect::<HashMap<_, _>>();
    let current_of = |saved: &SavedDisplay| present_by_uuid.get(saved.uuid.as_ref()?).copied();

    let spaces = snapshot
        .displays
        .iter()
        .filter_map(|saved| Some((saved, current_of(saved)?)))
        .flat_map(|(saved, (display_id, current))| {
            saved
                .workspace_ids
                .iter()
                .zip(current.iter())
                .map(move |(old, new)| (*old, (display_id, *new)))
        })
        .collect::<HashMap<_, _>>();

    PaneruState {
        active_display_id: snapshot.active_display_id.and_then(|active| {
            snapshot
                .displays
                .iter()
                .find(|saved| saved.display_id == active)
                .and_then(current_of)
                .map(|(display_id, _)| display_id)
        }),
        displays: snapshot
            .displays
            .iter()
            .filter_map(|saved| {
                let (display_id, current) = current_of(saved)?;
                Some(SavedDisplay {
                    display_id,
                    workspace_ids: current.clone(),
                    ..saved.clone()
                })
            })
            .collect(),
        workspaces: snapshot
            .workspaces
            .iter()
            .filter_map(|workspace| {
                let (display_id, workspace_id) = spaces.get(&workspace.workspace_id)?;
                Some(SavedWorkspace {
                    workspace_id: *workspace_id,
                    display_id: Some(*display_id),
                    ..workspace.clone()
                })
            })
            .collect(),
        ..snapshot.clone()
    }
}

fn width_ops(snapshot: &PaneruState) -> Vec<LayoutOp> {
    let windows = |column: &SavedColumn| -> Vec<SavedWindow> {
        match column {
            SavedColumn::Single(window) | SavedColumn::Fullscreen(window) => vec![window.clone()],
            SavedColumn::Tabs(windows) => windows.clone(),
            SavedColumn::Stack(items) => items
                .iter()
                .flat_map(|item| match item {
                    SavedStackItem::Single(window) => vec![window.clone()],
                    SavedStackItem::Tabs(windows) => windows.clone(),
                })
                .collect(),
        }
    };
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.strips)
        .flat_map(|strip| &strip.columns)
        .flat_map(windows)
        .filter_map(|window| {
            window.width_ratio.map(|ratio| LayoutOp::SetWidth {
                window: window.window_id,
                ratio,
            })
        })
        .collect()
}

#[allow(clippy::needless_pass_by_value)]
fn restore_setup(
    _: On<DisplaysSettled>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut setups: ResMut<DisplaySetups>,
    mut commands: Commands,
) {
    let Some(key) = setup_key(&window_manager) else {
        return;
    };
    if setups.settled.as_ref() == Some(&key) {
        return;
    }
    setups.settled = Some(key.clone());
    if !config.remember_display_setups() || !config.restore_enabled() {
        return;
    }
    let Some(snapshot) = setups.setups.get(&key) else {
        info!("No saved layout for display setup {key}");
        return;
    };

    let present = window_manager.present_displays();
    let remapped = remap_to_present(snapshot, &present, |id| window_manager.display_uuid(id));
    let widths = width_ops(&remapped);
    info!("Restoring the saved layout of display setup {key}");
    commands.insert_resource(SessionRestore::for_setup(
        remapped,
        config.restore_startup_grace(),
    ));
    commands.trigger(RestoreWindowState);
    if !widths.is_empty() {
        commands.trigger(SendMessageTrigger(Event::Command {
            command: Command::Layout(widths),
        }));
    }
}

#[cfg(test)]
mod tests {
    use bevy::math::IRect;

    use super::*;
    use crate::ecs::state::SavedRect;

    fn saved_display(
        display_id: CGDirectDisplayID,
        uuid: &str,
        spaces: &[WorkspaceId],
    ) -> SavedDisplay {
        SavedDisplay {
            display_id,
            bounds: SavedRect {
                min_x: 0,
                min_y: 0,
                max_x: 100,
                max_y: 100,
            },
            active: display_id == 1,
            workspace_ids: spaces.to_vec(),
            uuid: Some(uuid.to_string()),
        }
    }

    fn saved_workspace(workspace_id: WorkspaceId, display_id: CGDirectDisplayID) -> SavedWorkspace {
        SavedWorkspace {
            workspace_id,
            display_id: Some(display_id),
            active_virtual_index: Some(0),
            strips: Vec::new(),
        }
    }

    #[test]
    fn remap_follows_the_display_uuid_and_the_space_position() {
        let snapshot = PaneruState {
            version: 2,
            timestamp: 0,
            active_display_id: Some(7),
            displays: vec![
                saved_display(7, "monitor", &[311, 312]),
                saved_display(9, "projector", &[500]),
            ],
            workspaces: vec![
                saved_workspace(311, 7),
                saved_workspace(312, 7),
                saved_workspace(500, 9),
            ],
        };
        let monitor = Display::new(4, IRect::new(0, 0, 100, 100), 0);
        let present = vec![(monitor, vec![40, 41])];
        let uuid_of = |id| (id == 4).then(|| "monitor".to_string());

        let remapped = remap_to_present(&snapshot, &present, uuid_of);

        let spaces = remapped
            .workspaces
            .iter()
            .map(|workspace| (workspace.workspace_id, workspace.display_id))
            .collect::<Vec<_>>();
        assert_eq!(spaces, vec![(40, Some(4)), (41, Some(4))]);
        assert_eq!(remapped.active_display_id, Some(4));
        assert_eq!(
            remapped
                .displays
                .iter()
                .map(|display| display.display_id)
                .collect::<Vec<_>>(),
            vec![4]
        );
    }
}

use bevy::app::{App, PostUpdate, PreUpdate};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Added, Has};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::system::{Query, Res, ResMut};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use tracing::warn;

use super::{Command, Operation};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::Windows;
use crate::ecs::state::{PaneruActiveState, PaneruQueryState, PaneruVirtualWorkspaceState};
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, FocusedMarker, SelectedVirtualMarker, Unmanaged,
};
use crate::events::Event;
use crate::manager::{Application, Display, WindowManager};
use crate::platform::WinID;

#[derive(Default, Resource)]
struct StateSubscribers {
    streams: Vec<Arc<Mutex<UnixStream>>>,
}

#[derive(Default, Resource)]
struct StateBroadcastCache {
    workspace: Option<WorkspaceBroadcastSnapshot>,
    focus: Option<FocusBroadcastSnapshot>,
    virtual_workspaces: Option<Vec<PaneruVirtualWorkspaceState>>,
    titles: BTreeMap<WinID, String>,
}

#[derive(Clone, Debug, PartialEq)]
struct WorkspaceBroadcastSnapshot {
    display_id: Option<u32>,
    native_workspace_id: Option<u64>,
    virtual_workspace_number: Option<u32>,
}

impl From<&PaneruActiveState> for WorkspaceBroadcastSnapshot {
    fn from(active: &PaneruActiveState) -> Self {
        Self {
            display_id: active.display_id,
            native_workspace_id: active.native_workspace_id,
            virtual_workspace_number: active.virtual_workspace_number,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct FocusBroadcastSnapshot {
    window_id: Option<WinID>,
    bundle_id: Option<String>,
    title: Option<String>,
    virtual_workspace_number: Option<u32>,
}

impl From<&PaneruActiveState> for FocusBroadcastSnapshot {
    fn from(active: &PaneruActiveState) -> Self {
        Self {
            window_id: active.focused_window_id,
            bundle_id: active.focused_bundle_id.clone(),
            title: active.focused_window_title.clone(),
            virtual_workspace_number: active.virtual_workspace_number,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct StateBroadcastSignals {
    virtual_workspace_changed: bool,
    windows_changed: bool,
    window_focused: bool,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default, PartialEq)]
struct StateBroadcastIntent {
    virtual_workspace_changed: bool,
    windows_changed: bool,
    window_focused: bool,
    title_changes: BTreeSet<WinID>,
    display_changes: Vec<Option<u32>>,
    active_display_changed: bool,
}

impl StateBroadcastIntent {
    fn from_events<'a>(
        events: impl IntoIterator<Item = &'a Event>,
        signals: StateBroadcastSignals,
    ) -> Self {
        let mut intent = Self {
            virtual_workspace_changed: signals.virtual_workspace_changed,
            windows_changed: signals.windows_changed,
            window_focused: signals.window_focused,
            ..Self::default()
        };

        for event in events {
            match event {
                Event::SpaceChanged
                | Event::Command {
                    command: Command::Window(Operation::Virtual(_) | Operation::VirtualNumber(_)),
                } => intent.virtual_workspace_changed = true,
                Event::WindowCreated { .. }
                | Event::WindowDestroyed { .. }
                | Event::WindowMinimized { .. }
                | Event::WindowDeminimized { .. }
                | Event::Command {
                    command:
                        Command::Window(
                            Operation::VirtualMove(_, _) | Operation::VirtualMoveNumber(_, _),
                        ),
                } => intent.windows_changed = true,
                Event::WindowFocused { .. } => intent.window_focused = true,
                Event::WindowTitleChanged { window_id } => {
                    intent.title_changes.insert(*window_id);
                }
                Event::DisplayAdded { display_id }
                | Event::DisplayRemoved { display_id }
                | Event::DisplayMoved { display_id }
                | Event::DisplayResized { display_id }
                | Event::DisplayConfigured { display_id } => {
                    let display_id = Some(*display_id);
                    if !intent.display_changes.contains(&display_id) {
                        intent.display_changes.push(display_id);
                    }
                }
                Event::DisplayChanged => {
                    intent.active_display_changed = true;
                }
                _ => {}
            }
        }

        intent
    }

    fn requires_state(&self) -> bool {
        self.virtual_workspace_changed
            || self.windows_changed
            || self.window_focused
            || self.active_display_changed
    }

    fn is_empty(&self) -> bool {
        !self.requires_state() && self.title_changes.is_empty() && self.display_changes.is_empty()
    }
}

pub(super) fn register_query_commands(app: &mut App) {
    let active_subscribers = |subscribers: Option<Res<StateSubscribers>>| {
        subscribers.is_some_and(|subscribers| !subscribers.streams.is_empty())
    };

    app.init_resource::<StateSubscribers>();
    app.init_resource::<StateBroadcastCache>();
    app.add_systems(PreUpdate, (state_subscribe_handler, state_query_handler));
    app.add_systems(
        PostUpdate,
        state_event_broadcast_handler.run_if(active_subscribers),
    );
}

#[allow(clippy::needless_pass_by_value)]
fn state_query_handler(
    mut messages: MessageReader<Event>,
    workspaces: Query<(
        &ChildOf,
        &LayoutStrip,
        Has<ActiveWorkspaceMarker>,
        Has<SelectedVirtualMarker>,
    )>,
    displays: Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
    windows: Windows,
    apps: Query<&Application>,
    window_manager: Res<WindowManager>,
) {
    for event in messages.read() {
        let Event::StateQuery { kind, respond_to } = event else {
            continue;
        };

        let response =
            PaneruQueryState::extract(&workspaces, &displays, &windows, &apps, &window_manager)
                .map_err(|err| err.to_string())
                .and_then(|state| state.to_query_json(*kind).map_err(|err| err.to_string()))
                .unwrap_or_else(|err| json!({ "error": err }).to_string());
        _ = respond_to.send(response);
    }
}

#[allow(clippy::needless_pass_by_value)]
fn state_subscribe_handler(
    mut messages: MessageReader<Event>,
    mut subscribers: ResMut<StateSubscribers>,
) {
    for event in messages.read() {
        let Event::StateSubscribe { stream } = event else {
            continue;
        };
        subscribers.streams.push(stream.clone());
    }
}

#[cfg(test)]
fn collect_state_broadcast_events<'a>(
    events: impl IntoIterator<Item = &'a Event>,
    state: &PaneruQueryState,
    cache: &mut StateBroadcastCache,
    title_for_window: impl Fn(WinID) -> Option<String>,
    signals: StateBroadcastSignals,
) -> Vec<Value> {
    let intent = StateBroadcastIntent::from_events(events, signals);
    collect_state_broadcast_events_for_intent(&intent, Some(state), cache, title_for_window)
}

fn collect_state_broadcast_events_for_intent(
    intent: &StateBroadcastIntent,
    state: Option<&PaneruQueryState>,
    cache: &mut StateBroadcastCache,
    title_for_window: impl Fn(WinID) -> Option<String>,
) -> Vec<Value> {
    let mut display_changes = intent.display_changes.clone();
    if intent.active_display_changed
        && let Some(state) = state
        && !display_changes.contains(&state.active.display_id)
    {
        display_changes.push(state.active.display_id);
    }

    let mut title_changes = BTreeMap::new();
    for window_id in &intent.title_changes {
        title_changes.insert(*window_id, title_for_window(*window_id).unwrap_or_default());
    }

    let Some(state) = state else {
        let mut outgoing = Vec::new();
        for (window_id, title) in title_changes {
            if cache.titles.get(&window_id) == Some(&title) {
                continue;
            }
            outgoing.push(json!({
                "event": "window_title_changed",
                "window_id": window_id,
                "title": title,
            }));
            cache.titles.insert(window_id, title);
        }
        for display_id in display_changes {
            outgoing.push(json!({
                "event": "display_changed",
                "display_id": display_id,
            }));
        }
        return outgoing;
    };

    let mut outgoing = Vec::new();

    if intent.virtual_workspace_changed {
        let workspace = WorkspaceBroadcastSnapshot::from(&state.active);
        if cache.workspace.as_ref() != Some(&workspace)
            && (workspace.native_workspace_id.is_some()
                || workspace.virtual_workspace_number.is_some())
        {
            outgoing.push(json!({
                "event": "virtual_workspace_changed",
                "active": state.active.clone(),
            }));
            cache.workspace = Some(workspace);
        }
    }

    if intent.windows_changed
        && cache.virtual_workspaces.as_ref() != Some(&state.virtual_workspaces)
    {
        outgoing.push(json!({
            "event": "windows_changed",
            "virtual_workspace_number": state.active.virtual_workspace_number,
            "active": state.active.clone(),
        }));
        cache.virtual_workspaces = Some(state.virtual_workspaces.clone());
    }

    if intent.window_focused {
        let focus = FocusBroadcastSnapshot::from(&state.active);
        if focus.window_id.is_some() && cache.focus.as_ref() != Some(&focus) {
            outgoing.push(json!({
                "event": "window_focused",
                "window_id": focus.window_id,
                "bundle_id": focus.bundle_id,
                "title": focus.title,
                "virtual_workspace_number": focus.virtual_workspace_number,
            }));
            cache.focus = Some(focus);
        }
    }

    for (window_id, title) in title_changes {
        if cache.titles.get(&window_id) == Some(&title) {
            continue;
        }
        outgoing.push(json!({
            "event": "window_title_changed",
            "window_id": window_id,
            "title": title,
        }));
        cache.titles.insert(window_id, title);
    }

    for display_id in display_changes {
        outgoing.push(json!({
            "event": "display_changed",
            "display_id": display_id,
        }));
    }

    outgoing
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
fn state_event_broadcast_handler(
    mut messages: MessageReader<Event>,
    mut subscribers: ResMut<StateSubscribers>,
    mut cache: ResMut<StateBroadcastCache>,
    workspaces: Query<(
        &ChildOf,
        &LayoutStrip,
        Has<ActiveWorkspaceMarker>,
        Has<SelectedVirtualMarker>,
    )>,
    focused_changes: Query<Entity, Added<FocusedMarker>>,
    active_workspace_changes: Query<Entity, Added<ActiveWorkspaceMarker>>,
    displays: Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
    windows: Windows,
    apps: Query<&Application>,
    window_manager: Res<WindowManager>,
) {
    let events = messages.read().collect::<Vec<_>>();

    if subscribers.streams.is_empty() {
        return;
    }

    let signals = StateBroadcastSignals {
        virtual_workspace_changed: !active_workspace_changes.is_empty(),
        windows_changed: events.iter().any(|event| {
            let Event::WindowMoved { window_id } = event else {
                return false;
            };
            windows
                .find(*window_id)
                .and_then(|(_, entity)| windows.get_managed(entity))
                .is_some_and(|(_, _, unmanaged)| matches!(unmanaged, Some(Unmanaged::Floating)))
        }),
        window_focused: !focused_changes.is_empty(),
    };
    let intent = StateBroadcastIntent::from_events(events, signals);
    if intent.is_empty() {
        return;
    }

    let state = if intent.requires_state() {
        match PaneruQueryState::extract(&workspaces, &displays, &windows, &apps, &window_manager) {
            Ok(state) => Some(state),
            Err(err) => {
                warn!("extracting query state for broadcast: {err}");
                return;
            }
        }
    } else {
        None
    };
    let outgoing = collect_state_broadcast_events_for_intent(
        &intent,
        state.as_ref(),
        &mut cache,
        |window_id| {
            windows
                .find(window_id)
                .and_then(|(window, _)| window.title().ok())
        },
    );

    if outgoing.is_empty() {
        return;
    }

    let mut payload = outgoing
        .into_iter()
        .map(|event| event.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    payload.push('\n');

    subscribers.streams.retain(|stream| {
        let Ok(mut stream) = stream.lock() else {
            return false;
        };
        stream.write_all(payload.as_bytes()).is_ok()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::state::{PaneruVirtualWorkspaceState, PaneruWindowState};
    use crate::events::Event as PaneruEvent;

    fn query_state_with_active_window(
        window_id: WinID,
        bundle_id: &str,
        title: &str,
        virtual_workspace_number: u32,
        window_ids: Vec<WinID>,
    ) -> PaneruQueryState {
        let active = PaneruActiveState {
            display_id: Some(1),
            native_workspace_id: Some(10),
            virtual_workspace_number: Some(virtual_workspace_number),
            focused_window_id: Some(window_id),
            focused_bundle_id: Some(bundle_id.to_string()),
            focused_app_name: Some("Test App".to_string()),
            focused_window_title: Some(title.to_string()),
        };
        let windows = window_ids
            .into_iter()
            .map(|window_id| PaneruWindowState {
                window_id,
                bundle_id: bundle_id.to_string(),
                app_name: "Test App".to_string(),
                title: title.to_string(),
                focused: active.focused_window_id == Some(window_id),
                floating: false,
            })
            .collect();

        PaneruQueryState {
            version: 1,
            timestamp: 123,
            active,
            virtual_workspaces: vec![PaneruVirtualWorkspaceState {
                number: virtual_workspace_number,
                native_workspace_id: 10,
                active: true,
                windows,
            }],
        }
    }

    #[test]
    fn test_state_broadcast_coalesces_focus_events_to_current_state() {
        let state = query_state_with_active_window(
            26_261,
            "com.cmuxterm.app",
            "aicommit2 ~/P/nixos-config",
            2,
            vec![26_261],
        );
        let mut cache = StateBroadcastCache::default();
        let events = [
            PaneruEvent::WindowFocused { window_id: 18_639 },
            PaneruEvent::WindowFocused { window_id: 26_261 },
            PaneruEvent::WindowFocused { window_id: 26_261 },
        ];

        let outgoing = collect_state_broadcast_events(
            events.iter(),
            &state,
            &mut cache,
            |_| None,
            StateBroadcastSignals::default(),
        );

        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0]["event"], "window_focused");
        assert_eq!(outgoing[0]["window_id"], 26_261);
        assert_eq!(outgoing[0]["bundle_id"], "com.cmuxterm.app");
        assert_eq!(outgoing[0]["title"], "aicommit2 ~/P/nixos-config");

        let duplicate = collect_state_broadcast_events(
            events.iter(),
            &state,
            &mut cache,
            |_| None,
            StateBroadcastSignals::default(),
        );

        assert!(duplicate.is_empty());
    }

    #[test]
    fn test_state_broadcasts_floating_window_moves_and_skips_unchanged_state() {
        let state =
            query_state_with_active_window(26_261, "com.cmuxterm.app", "term", 2, vec![26_261]);
        let mut cache = StateBroadcastCache::default();
        let events = [PaneruEvent::WindowMoved { window_id: 26_261 }];
        let signals = StateBroadcastSignals {
            windows_changed: true,
            ..StateBroadcastSignals::default()
        };

        let outgoing =
            collect_state_broadcast_events(events.iter(), &state, &mut cache, |_| None, signals);

        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0]["event"], "windows_changed");
        assert_eq!(outgoing[0]["virtual_workspace_number"], 2);
        assert_eq!(outgoing[0]["active"]["focused_window_id"], 26_261);

        let duplicate =
            collect_state_broadcast_events(events.iter(), &state, &mut cache, |_| None, signals);

        assert!(duplicate.is_empty());

        let changed_state = query_state_with_active_window(
            26_261,
            "com.cmuxterm.app",
            "term",
            2,
            vec![26_261, 26_262],
        );
        let changed = collect_state_broadcast_events(
            events.iter(),
            &changed_state,
            &mut cache,
            |_| None,
            signals,
        );

        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0]["event"], "windows_changed");
    }

    #[test]
    fn test_state_broadcast_emits_focus_when_focused_marker_changes_without_event_message() {
        let state = query_state_with_active_window(
            26_262,
            "com.openai.codex",
            "Codex",
            2,
            vec![26_261, 26_262],
        );
        let mut cache = StateBroadcastCache::default();

        let outgoing = collect_state_broadcast_events(
            std::iter::empty(),
            &state,
            &mut cache,
            |_| None,
            StateBroadcastSignals {
                window_focused: true,
                ..StateBroadcastSignals::default()
            },
        );

        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0]["event"], "window_focused");
        assert_eq!(outgoing[0]["window_id"], 26_262);
        assert_eq!(outgoing[0]["bundle_id"], "com.openai.codex");
        assert_eq!(outgoing[0]["title"], "Codex");
    }

    #[test]
    fn test_state_broadcast_intent_skips_state_for_empty_or_unrelated_events() {
        let empty =
            StateBroadcastIntent::from_events(std::iter::empty(), StateBroadcastSignals::default());
        assert!(empty.is_empty());
        assert!(!empty.requires_state());

        let unrelated = StateBroadcastIntent::from_events(
            [
                PaneruEvent::ThemeChanged,
                PaneruEvent::WindowMoved { window_id: 10 },
                PaneruEvent::MouseUp {
                    point: objc2_core_foundation::CGPoint::default(),
                    modifiers: crate::platform::Modifiers::empty(),
                },
            ]
            .iter(),
            StateBroadcastSignals::default(),
        );
        assert!(unrelated.is_empty());
        assert!(!unrelated.requires_state());
    }

    #[test]
    fn test_state_broadcast_intent_classifies_relevant_events() {
        let intent = StateBroadcastIntent::from_events(
            [
                PaneruEvent::SpaceChanged,
                PaneruEvent::WindowMinimized { window_id: 10 },
                PaneruEvent::WindowFocused { window_id: 11 },
                PaneruEvent::WindowTitleChanged { window_id: 12 },
                PaneruEvent::DisplayResized { display_id: 2 },
            ]
            .iter(),
            StateBroadcastSignals::default(),
        );

        assert!(intent.virtual_workspace_changed);
        assert!(intent.windows_changed);
        assert!(intent.window_focused);
        assert_eq!(intent.title_changes, [12].into());
        assert_eq!(intent.display_changes, vec![Some(2)]);
        assert!(intent.requires_state());
        assert!(!intent.is_empty());
    }
}

//! Conversions between window-manager types and Lua values.
//!
//! [`LuaEvent`] mirrors the marshallable subset of [`Event`]; its serde tag
//! doubles as the `paneru.on` event name, so the registered name, the `type`
//! field on the table a handler receives, and the payload shape cannot drift
//! apart. [`LuaEvent::NAMES`] lists every emittable name, letting `paneru.on`
//! reject a typo at registration time.
//!
//! [`TryFrom<&Event>`] is exhaustive, so a new [`Event`] variant is a compile
//! error here until it's mapped or explicitly declared unmarshallable. It
//! reads the world and produces plain data; [`event_table`] separately turns
//! that into Lua values — only the first half needs the ECS and only the
//! second needs a [`Lua`], which is what lets the runtime live on a thread of
//! its own.

use mlua::{Lua, LuaSerdeExt, Table};
use serde::Serialize;

use crate::events::Event;
use crate::platform::{Modifiers, Pid, WinID, WorkspaceId};

/// The subset of [`Event`] that can be handed to Lua, with the payload each
/// carries. The serde tag doubles as the `paneru.on` event name.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LuaEvent {
    Exit,
    ProcessesLoaded,

    ApplicationActivated { pid: Pid },
    ApplicationDeactivated { pid: Pid },
    ApplicationVisible { pid: Pid },
    ApplicationHidden { pid: Pid },

    WindowSpawned(WindowSpawnPayload),
    WindowDestroyed { window_id: WinID },
    WindowFocused { window_id: WinID },
    WindowMoved { window_id: WinID },
    WindowResized { window_id: WinID },
    WindowMinimized { window_id: WinID },
    WindowDeminimized { window_id: WinID },
    WindowTitleChanged { window_id: WinID },

    MouseDown(MousePayload),
    MouseUp(MousePayload),
    MouseDragged(MousePayload),
    MouseMoved(MousePayload),

    Swipe { delta: f64, fingers: usize },
    VerticalSwipe { delta: f64, fingers: usize },
    VerticalScrollTick { delta: f64 },
    Scroll { delta: f64 },
    TouchpadDown,
    TouchpadUp,

    SpaceCreated { space_id: WorkspaceId },
    SpaceDestroyed { space_id: WorkspaceId },
    SpaceChanged,

    DisplayAdded { display_id: u32 },
    DisplayRemoved { display_id: u32 },
    DisplayMoved { display_id: u32 },
    DisplayResized { display_id: u32 },
    DisplayConfigured { display_id: u32 },
    DisplayChanged,

    MissionControlShowAllWindows,
    MissionControlShowFrontWindows,
    MissionControlShowDesktop,
    MissionControlExit,

    MenuOpened { window_id: WinID },
    MenuClosed { window_id: WinID },

    DockDidChangePref { message: String },
    DockDidRestart { message: String },
    MenuBarHiddenChanged { message: String },
    SystemWoke { message: String },

    ThemeChanged,
}

/// Pointer position and modifier state, flattened into the event table.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct MousePayload {
    pub x: f64,
    pub y: f64,
    pub modifiers: u32,
}

/// Enriched payload for [`LuaEvent::WindowSpawned`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowSpawnPayload {
    pub window_id: WinID,
    pub pid: Pid,
    pub app_name: String,
    pub bundle_id: String,
    pub title: String,
    pub frame: paneru_shared_types::state::Frame,
    pub floating: bool,
    pub managed: bool,
}

/// An [`Event`] that carries something Lua cannot see — an `AppKit` handle, a
/// socket, a config — or that is internal plumbing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotMarshallable;

impl TryFrom<&Event> for LuaEvent {
    type Error = NotMarshallable;

    /// Exhaustive on purpose: a new [`Event`] variant must decide here whether
    /// scripts can see it.
    #[allow(clippy::too_many_lines)]
    fn try_from(event: &Event) -> Result<Self, Self::Error> {
        let mouse = |point: &objc2_core_foundation::CGPoint, modifiers: Modifiers| MousePayload {
            x: point.x,
            y: point.y,
            modifiers: u32::from(modifiers.bits()),
        };

        Ok(match event {
            Event::Exit => LuaEvent::Exit,
            Event::ProcessesLoaded => LuaEvent::ProcessesLoaded,

            Event::ApplicationActivated { pid } => LuaEvent::ApplicationActivated { pid: *pid },
            Event::ApplicationDeactivated { pid } => LuaEvent::ApplicationDeactivated { pid: *pid },
            Event::ApplicationVisible { pid } => LuaEvent::ApplicationVisible { pid: *pid },
            Event::ApplicationHidden { pid } => LuaEvent::ApplicationHidden { pid: *pid },

            Event::WindowSpawned {
                window_id,
                pid,
                app_name,
                bundle_id,
                title,
                frame,
                floating,
                managed,
            } => LuaEvent::WindowSpawned(WindowSpawnPayload {
                window_id: *window_id,
                pid: *pid,
                app_name: app_name.clone(),
                bundle_id: bundle_id.clone(),
                title: title.clone(),
                frame: *frame,
                floating: *floating,
                managed: *managed,
            }),
            Event::WindowDestroyed { window_id, .. } => LuaEvent::WindowDestroyed {
                window_id: *window_id,
            },
            Event::WindowFocused { window_id } => LuaEvent::WindowFocused {
                window_id: *window_id,
            },
            Event::WindowMoved { window_id } => LuaEvent::WindowMoved {
                window_id: *window_id,
            },
            Event::WindowResized { window_id } => LuaEvent::WindowResized {
                window_id: *window_id,
            },
            Event::WindowMinimized { window_id } => LuaEvent::WindowMinimized {
                window_id: *window_id,
            },
            Event::WindowDeminimized { window_id } => LuaEvent::WindowDeminimized {
                window_id: *window_id,
            },
            Event::WindowTitleChanged { window_id } => LuaEvent::WindowTitleChanged {
                window_id: *window_id,
            },

            Event::MouseDown { point, modifiers } => LuaEvent::MouseDown(mouse(point, *modifiers)),
            Event::MouseUp { point, modifiers } => LuaEvent::MouseUp(mouse(point, *modifiers)),
            Event::MouseDragged { point, modifiers } => {
                LuaEvent::MouseDragged(mouse(point, *modifiers))
            }
            Event::MouseMoved { point, modifiers } => {
                LuaEvent::MouseMoved(mouse(point, *modifiers))
            }

            Event::Swipe { delta, fingers } => LuaEvent::Swipe {
                delta: *delta,
                fingers: *fingers,
            },
            Event::VerticalSwipe { delta, fingers } => LuaEvent::VerticalSwipe {
                delta: *delta,
                fingers: *fingers,
            },
            Event::VerticalScrollTick { delta } => LuaEvent::VerticalScrollTick { delta: *delta },
            Event::Scroll { delta } => LuaEvent::Scroll { delta: *delta },
            Event::TouchpadDown => LuaEvent::TouchpadDown,
            Event::TouchpadUp => LuaEvent::TouchpadUp,

            Event::SpaceCreated { space_id } => LuaEvent::SpaceCreated {
                space_id: *space_id,
            },
            Event::SpaceDestroyed { space_id } => LuaEvent::SpaceDestroyed {
                space_id: *space_id,
            },
            Event::SpaceChanged => LuaEvent::SpaceChanged,

            Event::DisplayAdded { display_id } => LuaEvent::DisplayAdded {
                display_id: *display_id,
            },
            Event::DisplayRemoved { display_id } => LuaEvent::DisplayRemoved {
                display_id: *display_id,
            },
            Event::DisplayMoved { display_id } => LuaEvent::DisplayMoved {
                display_id: *display_id,
            },
            Event::DisplayResized { display_id } => LuaEvent::DisplayResized {
                display_id: *display_id,
            },
            Event::DisplayConfigured { display_id } => LuaEvent::DisplayConfigured {
                display_id: *display_id,
            },
            Event::DisplayChanged => LuaEvent::DisplayChanged,

            Event::MissionControlShowAllWindows => LuaEvent::MissionControlShowAllWindows,
            Event::MissionControlShowFrontWindows => LuaEvent::MissionControlShowFrontWindows,
            Event::MissionControlShowDesktop => LuaEvent::MissionControlShowDesktop,
            Event::MissionControlExit => LuaEvent::MissionControlExit,

            Event::MenuOpened { window_id } => LuaEvent::MenuOpened {
                window_id: *window_id,
            },
            Event::MenuClosed { window_id } => LuaEvent::MenuClosed {
                window_id: *window_id,
            },

            Event::DockDidChangePref { msg } => LuaEvent::DockDidChangePref {
                message: msg.clone(),
            },
            Event::DockDidRestart { msg } => LuaEvent::DockDidRestart {
                message: msg.clone(),
            },
            Event::MenuBarHiddenChanged { msg } => LuaEvent::MenuBarHiddenChanged {
                message: msg.clone(),
            },
            Event::SystemWoke { msg } => LuaEvent::SystemWoke {
                message: msg.clone(),
            },

            Event::ThemeChanged => LuaEvent::ThemeChanged,

            // Non-marshallable payloads (AppKit handles, sockets, the config) or
            // internal plumbing.
            Event::InitialConfig(_)
            | Event::ConfigRefresh(_)
            | Event::ApplicationLaunched { .. }
            | Event::ApplicationTerminated { .. }
            | Event::ApplicationFrontSwitched { .. }
            | Event::WindowCreated { .. }
            | Event::Command { .. }
            | Event::StateQuery { .. }
            | Event::WindowSetQuery { .. }
            | Event::StateSubscribe { .. }
            | Event::ScriptState { .. } => return Err(NotMarshallable),
        })
    }
}

impl LuaEvent {
    /// Every name a script can register for with `paneru.on`. Checked against
    /// the variants by `every_emitted_name_is_registrable` below, so the two
    /// cannot drift.
    pub const NAMES: &'static [&'static str] = &[
        "exit",
        "processes_loaded",
        "application_activated",
        "application_deactivated",
        "application_visible",
        "application_hidden",
        "window_spawned",
        "window_destroyed",
        "window_focused",
        "window_moved",
        "window_resized",
        "window_minimized",
        "window_deminimized",
        "window_title_changed",
        "mouse_down",
        "mouse_up",
        "mouse_dragged",
        "mouse_moved",
        "swipe",
        "vertical_swipe",
        "vertical_scroll_tick",
        "scroll",
        "touchpad_down",
        "touchpad_up",
        "space_created",
        "space_destroyed",
        "space_changed",
        "display_added",
        "display_removed",
        "display_moved",
        "display_resized",
        "display_configured",
        "display_changed",
        "mission_control_show_all_windows",
        "mission_control_show_front_windows",
        "mission_control_show_desktop",
        "mission_control_exit",
        "menu_opened",
        "menu_closed",
        "dock_did_change_pref",
        "dock_did_restart",
        "menu_bar_hidden_changed",
        "system_woke",
        "theme_changed",
    ];

    /// Whether `name` is an event the runtime can actually emit.
    pub fn is_known(name: &str) -> bool {
        Self::NAMES.contains(&name)
    }
}

/// Marshals an already-extracted [`LuaEvent`] into `(name, table)` for dispatch
/// to `paneru.on` callbacks.
///
/// The dispatch name is the serde tag serde already wrote into the table's
/// `type` field, so there is no second hand-written variant→name mapping to
/// keep in step with it.
pub fn event_table(lua: &Lua, event: &LuaEvent) -> Option<(String, Table)> {
    let table = lua.to_value(event).ok()?.as_table().cloned()?;
    let name = table.get::<String>("type").ok()?;
    Some((name, table))
}

/// Extracts and marshals in one step. Convenience for callers that hold both an
/// [`Event`] and a [`Lua`]; the halves are separate for those that do not.
#[cfg(test)]
pub fn event_to_lua(lua: &Lua, event: &Event) -> Option<(String, Table)> {
    event_table(lua, &LuaEvent::try_from(event).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every event the runtime can emit, one per variant, for exhaustive checks.
    fn every_event() -> Vec<LuaEvent> {
        let mouse = MousePayload {
            x: 1.0,
            y: 2.0,
            modifiers: 0,
        };
        vec![
            LuaEvent::Exit,
            LuaEvent::ProcessesLoaded,
            LuaEvent::ApplicationActivated { pid: 1 },
            LuaEvent::ApplicationDeactivated { pid: 1 },
            LuaEvent::ApplicationVisible { pid: 1 },
            LuaEvent::ApplicationHidden { pid: 1 },
            LuaEvent::WindowSpawned(WindowSpawnPayload {
                window_id: 1,
                pid: 1,
                app_name: "test".into(),
                bundle_id: "test".into(),
                title: "test".into(),
                frame: paneru_shared_types::state::Frame {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
                floating: false,
                managed: true,
            }),
            LuaEvent::WindowDestroyed { window_id: 1 },
            LuaEvent::WindowFocused { window_id: 1 },
            LuaEvent::WindowMoved { window_id: 1 },
            LuaEvent::WindowResized { window_id: 1 },
            LuaEvent::WindowMinimized { window_id: 1 },
            LuaEvent::WindowDeminimized { window_id: 1 },
            LuaEvent::WindowTitleChanged { window_id: 1 },
            LuaEvent::MouseDown(mouse),
            LuaEvent::MouseUp(mouse),
            LuaEvent::MouseDragged(mouse),
            LuaEvent::MouseMoved(mouse),
            LuaEvent::Swipe {
                delta: 1.0,
                fingers: 3,
            },
            LuaEvent::VerticalSwipe {
                delta: 1.0,
                fingers: 3,
            },
            LuaEvent::VerticalScrollTick { delta: 1.0 },
            LuaEvent::Scroll { delta: 1.0 },
            LuaEvent::TouchpadDown,
            LuaEvent::TouchpadUp,
            LuaEvent::SpaceCreated { space_id: 1 },
            LuaEvent::SpaceDestroyed { space_id: 1 },
            LuaEvent::SpaceChanged,
            LuaEvent::DisplayAdded { display_id: 1 },
            LuaEvent::DisplayRemoved { display_id: 1 },
            LuaEvent::DisplayMoved { display_id: 1 },
            LuaEvent::DisplayResized { display_id: 1 },
            LuaEvent::DisplayConfigured { display_id: 1 },
            LuaEvent::DisplayChanged,
            LuaEvent::MissionControlShowAllWindows,
            LuaEvent::MissionControlShowFrontWindows,
            LuaEvent::MissionControlShowDesktop,
            LuaEvent::MissionControlExit,
            LuaEvent::MenuOpened { window_id: 1 },
            LuaEvent::MenuClosed { window_id: 1 },
            LuaEvent::DockDidChangePref {
                message: "m".into(),
            },
            LuaEvent::DockDidRestart {
                message: "m".into(),
            },
            LuaEvent::MenuBarHiddenChanged {
                message: "m".into(),
            },
            LuaEvent::SystemWoke {
                message: "m".into(),
            },
            LuaEvent::ThemeChanged,
        ]
    }

    /// The name an event dispatches under: the serde `type` tag serde writes
    /// into the table, which is exactly what `event_to_lua` keys dispatch on.
    fn serde_name(lua: &Lua, event: &LuaEvent) -> String {
        lua.to_value(event)
            .unwrap()
            .as_table()
            .cloned()
            .expect("events serialize to tables")
            .get::<String>("type")
            .unwrap()
    }

    #[test]
    fn every_emitted_name_is_registrable() {
        let lua = Lua::new();
        for event in every_event() {
            let name = serde_name(&lua, &event);
            assert!(
                LuaEvent::is_known(&name),
                "{name} is emitted but rejected by paneru.on"
            );
        }
        assert!(!LuaEvent::is_known("window_focussed"), "typos are rejected");
    }

    #[test]
    fn window_event_maps_to_named_table() {
        let lua = Lua::new();
        let (name, table) = event_to_lua(&lua, &Event::WindowFocused { window_id: 42 })
            .expect("window_focused should marshal");
        assert_eq!(name, "window_focused");
        assert_eq!(table.get::<String>("type").unwrap(), "window_focused");
        assert_eq!(table.get::<i64>("window_id").unwrap(), 42);
    }

    #[test]
    fn scalar_payloads_are_marshalled() {
        let lua = Lua::new();
        let (name, table) = event_to_lua(
            &lua,
            &Event::Swipe {
                delta: 1.5,
                fingers: 3,
            },
        )
        .expect("swipe should marshal");
        assert_eq!(name, "swipe");
        assert!((table.get::<f64>("delta").unwrap() - 1.5).abs() < f64::EPSILON);
        assert_eq!(table.get::<i64>("fingers").unwrap(), 3);
    }

    #[test]
    fn mouse_payloads_are_flattened_into_the_event() {
        let lua = Lua::new();
        let (name, table) = event_to_lua(
            &lua,
            &Event::MouseDown {
                point: objc2_core_foundation::CGPoint { x: 10.0, y: 20.0 },
                modifiers: Modifiers::ALT,
            },
        )
        .expect("mouse_down should marshal");
        assert_eq!(name, "mouse_down");
        assert!((table.get::<f64>("x").unwrap() - 10.0).abs() < f64::EPSILON);
        assert!((table.get::<f64>("y").unwrap() - 20.0).abs() < f64::EPSILON);
        assert_eq!(
            table.get::<i64>("modifiers").unwrap(),
            i64::from(Modifiers::ALT.bits())
        );
    }

    #[test]
    fn internal_events_are_skipped() {
        let lua = Lua::new();
        assert!(event_to_lua(&lua, &Event::SpaceChanged).is_some());
        // Internal plumbing / non-marshallable payloads yield no callback event.
        assert!(
            event_to_lua(
                &lua,
                &Event::Command {
                    command: crate::commands::Command::Lua(0),
                }
            )
            .is_none()
        );
    }
}

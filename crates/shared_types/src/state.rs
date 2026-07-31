//! The state documents Paneru answers queries with, and the events it pushes to
//! subscribers.
//!
//! Shared by the daemon and every client: the window manager fills these in from
//! its ECS world, the Lua module and any status bar deserialize the same types.
//!
//! They are the wire format of `paneru query …` and `paneru subscribe`, so
//! nobody has to poke at untyped JSON to read them.

use serde::{Deserialize, Serialize};

/// Which query a caller is asking for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StateQueryKind {
    /// The complete state document.
    State,
    /// Just the virtual workspace rows.
    VirtualWorkspaces,
    /// Just the active display/workspace/focus state.
    Active,
}

impl StateQueryKind {
    /// The argv token naming this query (`paneru query <token> --json`).
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            StateQueryKind::State => "state",
            StateQueryKind::VirtualWorkspaces => "virtual-workspaces",
            StateQueryKind::Active => "active",
        }
    }

    /// Parses the token back, so the socket and the CLI agree on the spelling.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        [
            StateQueryKind::State,
            StateQueryKind::VirtualWorkspaces,
            StateQueryKind::Active,
        ]
        .into_iter()
        .find(|kind| kind.token() == token)
    }
}

/// The complete state document (`paneru query state --json`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct QueryState {
    pub version: u32,
    pub timestamp: u64,
    pub active: ActiveState,
    pub virtual_workspaces: Vec<VirtualWorkspaceState>,
}

/// The active display, workspace and focused window.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ActiveState {
    pub display_id: Option<u32>,
    pub native_workspace_id: Option<u64>,
    pub virtual_workspace_number: Option<u32>,
    pub focused_window_id: Option<i32>,
    pub focused_bundle_id: Option<String>,
    pub focused_app_name: Option<String>,
    pub focused_window_title: Option<String>,
}

/// One virtual workspace row and the windows on it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VirtualWorkspaceState {
    pub number: u32,
    pub native_workspace_id: u64,
    pub active: bool,
    pub windows: Vec<WindowState>,
}

/// A window, as reported by queries and subscription events.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WindowState {
    pub window_id: i32,
    pub bundle_id: String,
    pub app_name: String,
    pub title: String,
    pub focused: bool,
    pub floating: bool,
}

impl QueryState {
    /// Serializes the slice of this document a query asked for.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails (it should not, barring a bug in
    /// this type's `Serialize` implementation).
    pub fn to_query_json(&self, kind: StateQueryKind) -> serde_json::Result<String> {
        match kind {
            StateQueryKind::State => serde_json::to_string(self),
            StateQueryKind::VirtualWorkspaces => serde_json::to_string(&self.virtual_workspaces),
            StateQueryKind::Active => serde_json::to_string(&self.active),
        }
    }
}

/// An event pushed to `paneru subscribe` clients, one JSON object per line.
///
/// The serde tag is the `event` field consumers switch on, so the name and the
/// payload have a single definition shared by the daemon that emits them and
/// the clients that read them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum StateEvent {
    /// The visible workspace changed (native Space or Paneru virtual row).
    VirtualWorkspaceChanged { active: ActiveState },
    /// The managed window list of the current workspace changed.
    WindowsChanged {
        virtual_workspace_number: Option<u32>,
        active: ActiveState,
    },
    /// Focus moved to another window.
    WindowFocused {
        window_id: Option<i32>,
        bundle_id: Option<String>,
        title: Option<String>,
        virtual_workspace_number: Option<u32>,
    },
    /// A window's title changed.
    WindowTitleChanged { window_id: i32, title: String },
    /// Display configuration changed. `display_id` is `null` for a global
    /// change Paneru cannot pin to one display.
    DisplayChanged { display_id: Option<u32> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_kinds_round_trip_through_their_tokens() {
        for kind in [
            StateQueryKind::State,
            StateQueryKind::VirtualWorkspaces,
            StateQueryKind::Active,
        ] {
            assert_eq!(StateQueryKind::parse(kind.token()), Some(kind));
        }
        assert_eq!(StateQueryKind::parse("nonsense"), None);
    }

    #[test]
    fn events_round_trip_through_json() {
        let event = StateEvent::WindowFocused {
            window_id: Some(1),
            bundle_id: Some("com.example.app".into()),
            title: Some("window".into()),
            virtual_workspace_number: Some(1),
        };

        let line = serde_json::to_string(&event).unwrap();
        assert!(line.contains(r#""event":"window_focused""#));
        assert_eq!(
            serde_json::from_str::<StateEvent>(&line).unwrap(),
            event,
            "clients must decode exactly what the daemon emits"
        );
    }
}

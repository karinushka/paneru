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
    /// Just the windows currently visible on screen.
    OnScreen,
}

impl StateQueryKind {
    /// Every query kind, in the order the CLI lists them.
    pub const ALL: [StateQueryKind; 4] = [
        StateQueryKind::State,
        StateQueryKind::VirtualWorkspaces,
        StateQueryKind::Active,
        StateQueryKind::OnScreen,
    ];

    /// The `paneru.query_*` shorthand each kind is exposed under in the Lua API,
    /// paired with the kind it queries. Both Lua hosts (the embedded runtime and
    /// the loadable client module) iterate this so the shorthand name and the
    /// kind it maps to are defined exactly once.
    pub const SHORTHANDS: [(&'static str, StateQueryKind); 4] = [
        ("query_state", StateQueryKind::State),
        ("query_active", StateQueryKind::Active),
        ("query_workspaces", StateQueryKind::VirtualWorkspaces),
        ("query_on_screen", StateQueryKind::OnScreen),
    ];

    /// The argv token naming this query (`paneru query <token> --json`).
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            StateQueryKind::State => "state",
            StateQueryKind::VirtualWorkspaces => "virtual-workspaces",
            StateQueryKind::Active => "active",
            StateQueryKind::OnScreen => "on-screen",
        }
    }

    /// Parses the token back, so the socket and the CLI agree on the spelling.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.token() == token)
    }

    /// Every token, comma separated, for "expected one of …" errors.
    #[must_use]
    pub fn tokens() -> String {
        Self::ALL
            .iter()
            .map(|kind| kind.token())
            .collect::<Vec<_>>()
            .join(", ")
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

/// A window frame in global display coordinates.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Frame {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
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
    /// Display the window is (mostly) on, when it overlaps one at all.
    pub display_id: Option<u32>,
    /// Frame in global display coordinates, when known.
    pub frame: Option<Frame>,
    /// Whether the window is meaningfully on screen right now: not minimized or
    /// hidden, and showing more than the sliver Paneru leaves poking out for
    /// off-screen windows.
    pub visible: bool,
}

impl QueryState {
    /// The windows currently on screen, left to right per display. Drawn from
    /// the same rows as the rest of the document — there is no separate
    /// on-screen state, only the visible subset of it.
    #[must_use]
    pub fn on_screen(&self) -> Vec<&WindowState> {
        let mut on_screen = self
            .virtual_workspaces
            .iter()
            .flat_map(|workspace| workspace.windows.iter())
            .filter(|window| window.visible)
            .collect::<Vec<_>>();
        on_screen.sort_by_key(|window| {
            (
                window.display_id,
                window.frame.map(|frame| frame.x),
                window.window_id,
            )
        });
        on_screen
    }

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
            StateQueryKind::OnScreen => serde_json::to_string(&self.on_screen()),
        }
    }

    /// The same slice as [`Self::to_query_json`], typed for the wire.
    ///
    /// This is what actually crosses between processes; the two JSON spellings
    /// above are for a terminal and for the embedded Lua runtime.
    #[must_use]
    pub fn to_query_payload(&self, kind: StateQueryKind) -> crate::wire::QueryPayload {
        use crate::wire::QueryPayload;
        match kind {
            StateQueryKind::State => QueryPayload::State(Box::new(self.clone())),
            StateQueryKind::VirtualWorkspaces => {
                QueryPayload::VirtualWorkspaces(self.virtual_workspaces.clone())
            }
            StateQueryKind::Active => QueryPayload::Active(Box::new(self.active.clone())),
            StateQueryKind::OnScreen => {
                QueryPayload::OnScreen(self.on_screen().into_iter().cloned().collect())
            }
        }
    }

    /// The same slice as [`Self::to_query_json`], left as a JSON value.
    ///
    /// In-process callers (the embedded Lua runtime) convert straight from this
    /// instead of serializing and parsing a string back.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails (it should not, barring a bug in
    /// this type's `Serialize` implementation).
    pub fn to_query_value(&self, kind: StateQueryKind) -> serde_json::Result<serde_json::Value> {
        match kind {
            StateQueryKind::State => serde_json::to_value(self),
            StateQueryKind::VirtualWorkspaces => serde_json::to_value(&self.virtual_workspaces),
            StateQueryKind::Active => serde_json::to_value(&self.active),
            StateQueryKind::OnScreen => serde_json::to_value(self.on_screen()),
        }
    }
}

/// An event pushed to `paneru subscribe` clients, one JSON object per line.
///
/// The serde tag is the `event` field consumers switch on, so the name and the
/// payload have a single definition shared by the daemon that emits them and
/// the clients that read them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
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
    /// The set of windows actually visible on screen changed — including plain
    /// moves and resizes, which no other event covers.
    OnScreenChanged {
        windows: Vec<WindowState>,
        active: ActiveState,
    },
    /// A window's title changed.
    WindowTitleChanged { window_id: i32, title: String },
    /// Display configuration changed. `display_id` is `null` for a global
    /// change Paneru cannot pin to one display.
    DisplayChanged { display_id: Option<u32> },
}

impl StateEvent {
    /// The documented `{"event": …}` JSON that `paneru subscribe --json` prints.
    ///
    /// # Errors
    ///
    /// If serialization fails, which should not happen barring a bug in this
    /// type's `Serialize` impl.
    pub fn to_json(&self) -> serde_json::Result<serde_json::Value> {
        Ok(crate::json::flatten_tag(
            serde_json::to_value(self)?,
            "event",
        ))
    }
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
            StateQueryKind::OnScreen,
        ] {
            assert_eq!(StateQueryKind::parse(kind.token()), Some(kind));
        }
        assert_eq!(StateQueryKind::parse("nonsense"), None);
    }

    #[test]
    fn events_round_trip_through_json() {
        let event = StateEvent::OnScreenChanged {
            windows: vec![WindowState {
                window_id: 1,
                bundle_id: "com.example.app".into(),
                app_name: "Example".into(),
                title: "window".into(),
                focused: true,
                floating: false,
                display_id: Some(1),
                frame: Some(Frame {
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                }),
                visible: true,
            }],
            active: ActiveState::default(),
        };

        let line = serde_json::to_string(&event.to_json().unwrap()).unwrap();
        assert!(line.contains(r#""event":"on_screen_changed""#));
        assert!(line.contains(r#""windows":"#));

        let bytes = postcard::to_allocvec(&event).unwrap();
        assert_eq!(
            postcard::from_bytes::<StateEvent>(&bytes).unwrap(),
            event,
            "clients must decode exactly what the daemon emits"
        );
    }

    #[test]
    fn on_screen_is_the_visible_subset_ordered_left_to_right() {
        let window = |window_id, x, visible| WindowState {
            window_id,
            bundle_id: String::new(),
            app_name: String::new(),
            title: String::new(),
            focused: false,
            floating: false,
            display_id: Some(1),
            frame: Some(Frame {
                x,
                y: 0,
                width: 100,
                height: 100,
            }),
            visible,
        };

        let state = QueryState {
            version: 1,
            timestamp: 0,
            active: ActiveState::default(),
            virtual_workspaces: vec![VirtualWorkspaceState {
                number: 1,
                native_workspace_id: 1,
                active: true,
                windows: vec![
                    window(1, 500, true),
                    window(2, 0, false),
                    window(3, 100, true),
                ],
            }],
        };

        let visible: Vec<i32> = state
            .on_screen()
            .iter()
            .map(|window| window.window_id)
            .collect();
        assert_eq!(visible, vec![3, 1], "off-screen windows are left out");
    }
}

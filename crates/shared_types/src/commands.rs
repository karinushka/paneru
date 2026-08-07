//! The Paneru command vocabulary.
//!
//! Every way of telling the window manager to do something funnels through
//! [`Command`]: the TOML `[bindings]` table, the `send-cmd` socket protocol, an
//! embedded Lua `init.lua`, and the loadable Lua client module. This crate owns
//! the types and their argv encoding ([`parse_command`] / [`Command::to_argv`]).

use serde::{Deserialize, Serialize};

pub use crate::argv::{ParseError, parse_command};

/// Represents a cardinal or directional choice for window manipulation.
///
/// Deserializes from either a direction name (`"east"`) or a 1-based position
/// (`3`, meaning the third column), which is what both the argv encoding and the
/// Lua API accept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    North,
    South,
    West,
    East,
    First,
    Last,
    Nth(usize),
}

impl Direction {
    #[must_use]
    pub fn reverse(&self) -> Self {
        match self {
            Direction::North => Direction::South,
            Direction::South => Direction::North,
            Direction::West => Direction::East,
            Direction::East => Direction::West,
            Direction::First => Direction::Last,
            Direction::Last => Direction::First,
            Direction::Nth(index) => Direction::Nth(*index),
        }
    }

    /// Parses a direction name. Positions are not accepted; see
    /// [`Direction::parse_positional`].
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] if `name` is not a recognized direction name.
    pub fn parse(name: &str) -> Result<Self, ParseError> {
        Ok(match name {
            "north" => Direction::North,
            "south" => Direction::South,
            "west" => Direction::West,
            "east" => Direction::East,
            "first" => Direction::First,
            "last" => Direction::Last,
            other => return Err(ParseError::new(format!("unhandled direction '{other}'"))),
        })
    }

    /// Parses a direction name or a 1-based position (`"3"` → `Nth(2)`).
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] if `input` is neither a recognized direction name
    /// nor a positive integer.
    pub fn parse_positional(input: &str) -> Result<Self, ParseError> {
        match input.parse::<usize>() {
            Ok(0) => Err(ParseError::new("window numbers start at 1")),
            Ok(number) => Ok(Direction::Nth(number - 1)),
            Err(_) => Direction::parse(input),
        }
    }

    /// The argv token this direction encodes to.
    #[must_use]
    pub fn token(&self) -> String {
        match self {
            Direction::North => "north".into(),
            Direction::South => "south".into(),
            Direction::West => "west".into(),
            Direction::East => "east".into(),
            Direction::First => "first".into(),
            Direction::Last => "last".into(),
            Direction::Nth(index) => (index + 1).to_string(),
        }
    }
}

/// The plain, externally tagged spelling of [`Direction`], used on the wire.
///
/// A binary format cannot decode the flexible `"east"`-or-`3` form below —
/// `untagged` works by asking the format what the next value *is*, which only a
/// self-describing one can answer. This mirror carries the same variants with a
/// derived impl, so the wire gets a discriminant and a payload.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DirectionRepr {
    North,
    South,
    West,
    East,
    First,
    Last,
    Nth(usize),
}

impl From<DirectionRepr> for Direction {
    fn from(repr: DirectionRepr) -> Self {
        match repr {
            DirectionRepr::North => Self::North,
            DirectionRepr::South => Self::South,
            DirectionRepr::West => Self::West,
            DirectionRepr::East => Self::East,
            DirectionRepr::First => Self::First,
            DirectionRepr::Last => Self::Last,
            DirectionRepr::Nth(index) => Self::Nth(index),
        }
    }
}

impl<'de> Deserialize<'de> for Direction {
    /// From a human-readable format, accepts `"east"` or `3`, so a Lua caller
    /// can write either `{ direction = "east" }` or `{ number = 3 }` and get the
    /// same enum. From a binary one — the wire — reads the derived spelling,
    /// because the flexible form is undecodable there and unnecessary: nothing
    /// hand-writes a request.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// The two spellings a human-readable format accepts.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Name(String),
            Position(i64),
        }

        if !deserializer.is_human_readable() {
            return DirectionRepr::deserialize(deserializer).map(Direction::from);
        }

        match Repr::deserialize(deserializer)? {
            Repr::Name(name) => Direction::parse(&name).map_err(serde::de::Error::custom),
            Repr::Position(number) => usize::try_from(number)
                .ok()
                .filter(|number| *number > 0)
                .map(|number| Direction::Nth(number - 1))
                .ok_or_else(|| serde::de::Error::custom("window numbers start at 1")),
        }
    }
}

/// Direction used when cycling preset resize widths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeDirection {
    Grow,
    Shrink,
}

impl ResizeDirection {
    /// # Errors
    ///
    /// Returns [`ParseError`] if `input` is not a recognized resize direction.
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        Ok(match input {
            "grow" => ResizeDirection::Grow,
            "shrink" => ResizeDirection::Shrink,
            other => {
                return Err(ParseError::new(format!(
                    "unhandled resize direction '{other}'"
                )));
            }
        })
    }

    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            ResizeDirection::Grow => "grow",
            ResizeDirection::Shrink => "shrink",
        }
    }
}

/// Controls whether focus follows the window after a move operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveFocus {
    Follow,
    Stay,
}

impl MoveFocus {
    /// `follow = true` is the default everywhere a caller can choose.
    #[must_use]
    pub fn follows(follow: bool) -> Self {
        if follow { Self::Follow } else { Self::Stay }
    }
}

/// A 1-based virtual workspace number as written by users, stored 0-based.
///
/// # Errors
///
/// Returns [`ParseError`] if `input` is not a positive integer.
pub fn parse_virtual_workspace_number(input: &str) -> Result<u32, ParseError> {
    let number = input
        .parse::<u32>()
        .map_err(|_| ParseError::new(format!("unhandled virtual workspace '{input}'")))?;
    if number == 0 {
        return Err(ParseError::new("virtual workspace numbers start at 1"));
    }
    Ok(number - 1)
}

/// Defines the various operations that can be performed on windows.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    /// Focuses on a window in the specified `Direction`.
    Focus(Direction),
    /// Swaps the current window with another in the specified `Direction`.
    Swap(Direction),
    /// Centers the currently focused window on the display.
    Center,
    /// Resizes the focused window in the given direction.
    Resize(ResizeDirection),
    /// Resizes the focused window to an exact display-width ratio.
    SetWidth(f64),
    /// Toggles the focused window to full width or a preset width.
    FullWidth,
    /// Moves the focused window to the next available display.
    ToNextDisplay(MoveFocus),
    /// Distributes heights equally among windows in the focused stack.
    Equalize,
    /// Makes all columns in the active strip the same width as the focused window.
    Balance,
    /// Toggles the managed state of the focused window.
    Manage,
    /// Stacks or unstacks a window. The boolean indicates whether to stack (`true`) or unstack (`false`).
    Stack(bool),
    /// Resizes and repositions the focused window to fit within the visible viewport
    /// (including edge padding).
    Snap,
    /// Cyclically selects the virtual strip for the current workspace.
    Virtual(Direction),
    /// Selects a virtual strip by its zero-based index for the current workspace.
    VirtualNumber(u32),
    /// Creates a new empty virtual strip after the highest existing one for
    /// the current workspace, and switches to it.
    VirtualAdd,
    /// Moves the focused window to the virtual strip.
    VirtualMove(Direction, MoveFocus),
    /// Moves the focused window to a virtual strip by its zero-based index.
    VirtualMoveNumber(u32, MoveFocus),
    /// Focuses the workspace's last-focused floating window.
    FocusUnmanaged,
    /// Focuses the workspace's last-focused managed (tiled) window.
    FocusManaged,
    /// Raises all visible floating windows on the active display and focuses
    /// the last-floating window (idempotent — repeat presses behave the same).
    RaiseFloating,
    /// Alt-tab between the floating and tiled tiers of the active workspace.
    /// Flips `FloatingLayer`, raises the other windows in the new top tier,
    /// and focuses the tier's last-focused window.
    ToggleFloatingLayer,
}

/// Defines operations that can be performed on the mouse.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseMove {
    /// Moves the mouse pointer to the next available display.
    ToNextDisplay,
}

/// Represents a command that can be issued to the window manager.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    /// A command targeting a window with a specific `Operation`.
    Window(Operation),
    /// A command targeting the mouse with a specific `MouseOperation`.
    Mouse(MouseMove),
    /// A command to quit the window manager application.
    Quit,
    /// A command to restart the window manager service.
    Restart,
    PrintState,
    /// Invokes a Lua keybind handler by its registry id (see the daemon's
    /// `crate::lua`). Never produced by parsing; the runtime issues it directly.
    Lua(u32),
    /// Layout operations a Lua handler produced by transforming a `WindowSet`.
    /// Window-addressed, unlike every other command here, and applied
    /// best-effort: see `ecs::layout_ops`. Never produced by parsing.
    Layout(Vec<crate::windowset::LayoutOp>),
}

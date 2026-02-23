use arc_swap::{ArcSwap, Guard};
use bevy::ecs::resource::Resource;
use objc2_core_foundation::{CFData, CFString};
use regex::Regex;
use serde::{Deserialize, Deserializer, de};
use std::{
    collections::HashMap,
    env,
    ffi::c_void,
    fs::read_to_string,
    path::{Path, PathBuf},
    ptr::NonNull,
    sync::{Arc, LazyLock},
};
use stdext::function_name;
use tracing::{error, info, warn};

use crate::{
    commands::{Command, Direction, MouseMove, Operation},
    platform::{Modifiers, OSStatus},
};
use crate::{
    errors::{Error, Result},
    util::MacResult,
};
use crate::{platform::CFStringRef, util::AXUIWrapper};

/// A `LazyLock` that determines the path to the application's configuration file.
/// It checks the `PANERU_CONFIG` environment variable first, then standard XDG locations and user home directory.
/// If no configuration file is found, the application will panic.
pub static CONFIGURATION_FILE: LazyLock<PathBuf> = LazyLock::new(|| {
    if let Ok(path_str) = env::var("PANERU_CONFIG") {
        let path = PathBuf::from(path_str);
        if path.exists() {
            return path;
        }
        warn!(
            "{}: $PANERU_CONFIG is set to {}, but the file does not exist. Falling back to default locations.",
            function_name!(),
            path.display()
        );
    }

    let standard_paths = [
        env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".paneru")),
        env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".paneru.toml")),
        env::var("XDG_CONFIG_HOME")
            .ok()
            .map(|x| PathBuf::from(x).join("paneru/paneru.toml")),
    ];

    standard_paths
        .into_iter()
        .flatten()
        .find(|path| path.exists())
        .unwrap_or_else(|| {
            panic!(
                "{}: Configuration file not found. Tried: $PANERU_CONFIG, $HOME/.paneru, $HOME/.paneru.toml, $XDG_CONFIG_HOME/paneru/paneru.toml",
                function_name!()
            )
        })
});

/// Parses a string into a `Direction` enum.
///
/// # Arguments
///
/// * `dir` - The string representation of the direction (e.g., "north", "west").
///
/// # Returns
///
/// `Ok(Direction)` if the string is a valid direction, otherwise `Err(Error::InvalidConfig)`.
fn parse_direction(dir: &str) -> Result<Direction> {
    Ok(match dir {
        "north" => Direction::North,
        "south" => Direction::South,
        "west" => Direction::West,
        "east" => Direction::East,
        "first" => Direction::First,
        "last" => Direction::Last,
        _ => {
            return Err(Error::InvalidConfig(format!(
                "{}: Unhandled direction {dir}",
                function_name!()
            )));
        }
    })
}

/// Parses a command argument vector into an `Operation` enum.
///
/// # Arguments
///
/// * `argv` - A slice of strings representing the command arguments (e.g., `["focus", "east"]`).
///
/// # Returns
///
/// `Ok(Operation)` if the arguments represent a valid operation, otherwise `Err(Error::InvalidConfig)`.
fn parse_operation(argv: &[&str]) -> Result<Operation> {
    let empty = "";
    let cmd = *argv.first().unwrap_or(&empty);
    let err = Error::InvalidConfig(format!("{}: Invalid command '{argv:?}'", function_name!()));

    let out = match cmd {
        "focus" => Operation::Focus(parse_direction(argv.get(1).ok_or(err)?)?),
        "swap" => Operation::Swap(parse_direction(argv.get(1).ok_or(err)?)?),
        "center" => Operation::Center,
        "resize" => Operation::Resize,
        "fullwidth" => Operation::FullWidth,
        "manage" => Operation::Manage,
        "equalize" => Operation::Equalize,
        "stack" => Operation::Stack(true),
        "unstack" => Operation::Stack(false),
        "nextdisplay" => Operation::ToNextDisplay,
        _ => {
            return Err(err);
        }
    };
    Ok(out)
}

/// Parses a command argument vector into a `MouseMove` enum.
fn parse_mouse_move(argv: &[&str]) -> Result<MouseMove> {
    let empty = "";
    let cmd = *argv.first().unwrap_or(&empty);
    let err = Error::InvalidConfig(format!(
        "{}: Invalid mouse command '{argv:?}'",
        function_name!()
    ));

    let out = match cmd {
        "nextdisplay" => MouseMove::ToNextDisplay,
        _ => {
            return Err(err);
        }
    };
    Ok(out)
}

/// Parses a command argument vector into a `Command` enum.
///
/// # Arguments
///
/// * `argv` - A slice of strings representing the command arguments (e.g., `["window", "focus", "east"]`).
///
/// # Returns
///
/// `Ok(Command)` if the arguments represent a valid command, otherwise `Err(Error::InvalidConfig)`.
pub fn parse_command(argv: &[&str]) -> Result<Command> {
    let empty = "";
    let cmd = *argv.first().unwrap_or(&empty);

    let out = match cmd {
        "printstate" => Command::PrintState,
        "window" => Command::Window(parse_operation(&argv[1..])?),
        "mouse" => Command::Mouse(parse_mouse_move(&argv[1..])?),
        "quit" => Command::Quit,
        _ => {
            return Err(Error::InvalidConfig(format!(
                "{}: Unhandled command '{argv:?}'",
                function_name!()
            )));
        }
    };
    Ok(out)
}

/// `Config` manages the application's configuration, including options, keybindings, and window-specific parameters.
/// It provides methods for loading, reloading, and querying configuration settings.
#[derive(Clone, Debug, Resource)]
pub struct Config {
    inner: Arc<ArcSwap<InnerConfig>>,
}

impl Config {
    /// Creates a new `Config` instance by loading the configuration from the specified path.
    ///
    /// # Arguments
    ///
    /// * `path` - A reference to the path of the configuration file.
    ///
    /// # Returns
    ///
    /// `Ok(Self)` if the configuration is loaded successfully, otherwise `Err(Error)` with an error message.
    pub fn new(path: &Path) -> Result<Self> {
        let input = read_to_string(path)?;
        Ok(Config {
            inner: Arc::new(ArcSwap::from_pointee(InnerConfig::new(&input)?)),
        })
    }

    /// Reloads the configuration from the specified path, updating the internal options and keybindings.
    ///
    /// # Arguments
    ///
    /// * `path` - A reference to the path of the new configuration file.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the configuration is reloaded successfully, otherwise `Err(Error)` with an error message.
    pub fn reload_config(&mut self, path: &Path) -> Result<()> {
        let input = read_to_string(path)?;
        let new = InnerConfig::new(&input)?;
        self.inner.store(Arc::new(new));
        Ok(())
    }

    /// Returns a read guard to the inner `InnerConfig` for read-only access.
    ///
    /// # Returns
    ///
    /// A `Guard<Arc<InnerConfig>>` allowing read access to `InnerConfig`.
    fn inner(&self) -> Guard<Arc<InnerConfig>> {
        self.inner.load()
    }

    /// Returns a clone of the `MainOptions` from the current configuration.
    ///
    /// # Returns
    ///
    /// A `MainOptions` struct containing the main configuration options.
    pub fn options(&self) -> MainOptions {
        self.inner().options.clone()
    }

    // Returns the animation speed in 1/10th of screen size per second.
    // E.g. a value of 20 means: move at a speed of two screen sizes per second.
    // If a screen is 1920px, the calculated speed would be 3840 pixels/second.
    pub fn animation_speed(&self) -> f64 {
        self.options()
            .animation_speed
            // If unset, set it to something high, so the move happens immediately,
            // effectively disabling animation.
            .unwrap_or(1_000_000.0)
            .max(5.0)
            / 10.0
    }

    /// Finds a keybinding matching the given `keycode` and `modifier` mask.
    ///
    /// # Arguments
    ///
    /// * `keycode` - The raw key code of the keybinding to find.
    /// * `mask` - The modifier mask (e.g., `Alt`, `Shift`, `Cmd`, `Ctrl`) of the keybinding.
    ///
    /// # Returns
    ///
    /// `Some(Command)` if a matching keybinding is found, otherwise `None`.
    pub fn find_keybind(&self, keycode: u8, mask: &Modifiers) -> Option<Command> {
        let config = self.inner();
        config
            .bindings
            .values()
            .flat_map(|binds| binds.all())
            .find_map(|bind| {
                (bind.code == keycode && bind.modifiers == *mask).then_some(bind.command.clone())
            })
    }

    /// Finds window properties for a given `title` and `bundle_id`.
    /// It iterates through configured window parameters and returns the first match.
    ///
    /// # Arguments
    ///
    /// * `title` - The title of the window to match.
    /// * `bundle_id` - The bundle identifier of the application owning the window.
    ///
    /// # Returns
    ///
    /// `Some(WindowParams)` if matching window properties are found, otherwise `None`.
    pub fn find_window_properties(&self, title: &str, bundle_id: &str) -> Vec<WindowParams> {
        self.inner()
            .windows
            .as_ref()
            .map(|windows| {
                windows
                    .values()
                    .filter(|params| {
                        let bundle_match =
                            params.bundle_id.as_ref().map(|id| id.as_str() == bundle_id);
                        bundle_match.is_none_or(|m| m) && params.title.is_match(title)
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    pub fn sliver_height(&self) -> f64 {
        self.options().sliver_height.unwrap_or(1.0).clamp(0.1, 1.0)
    }

    pub fn sliver_width(&self) -> i32 {
        i32::from(self.options().sliver_width.unwrap_or(5)).max(1)
    }

    pub fn edge_padding(&self) -> (i32, i32, i32, i32) {
        let o = self.options();
        (
            i32::from(o.padding_top.unwrap_or(0)),
            i32::from(o.padding_right.unwrap_or(0)),
            i32::from(o.padding_bottom.unwrap_or(0)),
            i32::from(o.padding_left.unwrap_or(0)),
        )
    }

    pub fn preset_column_widths(&self) -> Vec<f64> {
        self.options().preset_column_widths
    }
}

impl Default for Config {
    /// Returns a default `Config` instance with an empty `InnerConfig`.
    fn default() -> Self {
        Config {
            inner: Arc::new(ArcSwap::from_pointee(InnerConfig::default())),
        }
    }
}

impl TryFrom<&str> for Config {
    type Error = crate::errors::Error;

    fn try_from(input: &str) -> std::result::Result<Self, Self::Error> {
        Ok(Config {
            inner: Arc::new(ArcSwap::from_pointee(InnerConfig::new(input)?)),
        })
    }
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum OneOrMore {
    Single(Keybinding),
    Multiple(Vec<Keybinding>),
}

impl OneOrMore {
    fn all(&self) -> Vec<&Keybinding> {
        match self {
            OneOrMore::Single(one) => vec![one],
            OneOrMore::Multiple(many) => many.iter().collect::<Vec<_>>(),
        }
    }

    fn all_mut(&mut self) -> Vec<&mut Keybinding> {
        match self {
            OneOrMore::Single(one) => vec![one],
            OneOrMore::Multiple(many) => many.iter_mut().collect::<Vec<_>>(),
        }
    }
}

/// `InnerConfig` holds the actual configuration data parsed from a file, including options, keybindings, and window parameters.
/// It is typically accessed via an `Arc<RwLock<InnerConfig>>` within the `Config` struct.
#[derive(Deserialize, Debug, Default)]
struct InnerConfig {
    options: MainOptions,
    bindings: HashMap<String, OneOrMore>,
    windows: Option<HashMap<String, WindowParams>>,
}

impl InnerConfig {
    /// Creates a new `InnerConfig` by reading and parsing the configuration file from the specified `path`.
    ///
    /// # Arguments
    ///
    /// * `path` - A reference to the path of the configuration file.
    ///
    /// # Returns
    ///
    /// `Ok(InnerConfig)` if the configuration is parsed successfully, otherwise `Err(Error)` with an error message.
    fn new(input: &str) -> Result<InnerConfig> {
        InnerConfig::parse_config(input)
    }

    /// Parses the configuration from a string `input`.
    /// It populates the `code` and `command` fields of `Keybinding` by looking up virtual keys and literal keycodes.
    ///
    /// # Arguments
    ///
    /// * `input` - The string content of the configuration file.
    ///
    /// # Returns
    ///
    /// `Ok(InnerConfig)` if the parsing is successful, otherwise `Err(Error)` with an error message.
    fn parse_config(input: &str) -> Result<InnerConfig> {
        let virtual_keys = generate_virtual_keymap();
        let mut config: InnerConfig = toml::from_str(input)?;

        for (command, bindings) in &mut config.bindings {
            let argv = command.split('_').collect::<Vec<_>>();
            for binding in bindings.all_mut() {
                binding.command = parse_command(&argv)?;

                let code = virtual_keys
                    .iter()
                    .find(|(key, _)| key == &binding.key)
                    .map(|(_, code)| *code)
                    .or_else(|| {
                        literal_keycode()
                            .find(|(key, _)| key == &binding.key)
                            .map(|(_, code)| *code)
                    });
                if let Some(code) = code {
                    binding.code = code;
                } else {
                    error!("{}: invalid key '{}'", function_name!(), &binding.key);
                }
                info!("bind: {binding:?}");
            }
        }
        Ok(config)
    }
}

/// `MainOptions` represents the primary configuration options for the window manager.
/// These options control various behaviors such as mouse focus, gesture recognition, and window animation.
#[derive(Deserialize, Clone, Debug, Default)]
pub struct MainOptions {
    /// Enables or disables focus follows mouse behavior.
    pub focus_follows_mouse: Option<bool>,
    /// Enables or disables mouse follows focus behavior.
    pub mouse_follows_focus: Option<bool>,
    /// The number of fingers required for swipe gestures to move windows.
    pub swipe_gesture_fingers: Option<usize>,
    /// A list of preset column widths (as ratios) used for resizing windows.
    #[serde(default = "default_preset_column_widths")]
    pub preset_column_widths: Vec<f64>,
    /// The animation speed for window movements in pixels per second.
    pub animation_speed: Option<f64>,
    /// Automatically center the window when switching focus with keyboard.
    pub auto_center: Option<bool>,
    /// Height of off-screen window slivers as a ratio (0.0–1.0) of the display height.
    /// Lower values hide the window's corner radius at screen edges.
    /// Default: 1.0 (full height).
    pub sliver_height: Option<f64>,
    /// Width of off-screen window slivers in pixels.
    /// Default: 5 pixels.
    pub sliver_width: Option<u16>,
    /// Padding applied at screen edges (in pixels). Independent from between-window gaps.
    /// Default: 0 on all sides.
    pub padding_top: Option<u16>,
    pub padding_bottom: Option<u16>,
    pub padding_left: Option<u16>,
    pub padding_right: Option<u16>,

    #[allow(dead_code)]
    pub continuous_swipe: Option<bool>, // Deprecated
}

/// Returns a default set of column widths.
pub fn default_preset_column_widths() -> Vec<f64> {
    vec![0.25, 0.33333, 0.50, 0.66667, 0.75]
}

/// `Keybinding` represents a keyboard shortcut and the command it triggers.
/// It includes the key, its raw keycode, modifier keys, and the associated command.
#[derive(Debug)]
pub struct Keybinding {
    pub key: String,
    pub code: u8,
    pub modifiers: Modifiers,
    pub command: Command,
}

impl<'de> Deserialize<'de> for Keybinding {
    /// Deserializes a `Keybinding` from a string input. The input string is expected to be in a format like "`modifier+modifier-key`" or "`key`".
    /// Examples: "`ctrl+alt-q`", "`shift-tab`", "`h`".
    ///
    /// # Arguments
    ///
    /// * `deserializer` - The deserializer used to parse the input.
    ///
    /// # Returns
    ///
    /// `Ok(Self)` if the deserialization is successful, otherwise `Err(D::Error)` with a custom error message.
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let input = String::deserialize(deserializer)?;
        let mut parts = input.split('-').map(str::trim).collect::<Vec<_>>();
        let key = parts.pop();

        if parts.len() > 1 || key.is_none() {
            return Err(de::Error::custom(format!("Too many dashes: {input:?}")));
        }

        let modifiers = match parts.pop() {
            Some(modifiers) => parse_modifiers(modifiers).map_err(de::Error::custom)?,
            None => Modifiers::empty(),
        };

        Ok(Keybinding {
            key: key.unwrap().to_string(),
            code: 0,
            modifiers,
            command: Command::Quit,
        })
    }
}

/// `WindowParams` defines rules and properties for specific windows based on their title or bundle ID.
/// These parameters can override default window management behavior, such as forcing a window to float or setting its initial index.
#[derive(Clone, Debug, Deserialize)]
pub struct WindowParams {
    /// A regular expression to match against the window's title.
    #[serde(deserialize_with = "deserialize_title")]
    title: Regex,
    /// An optional bundle identifier to match against the application's bundle ID.
    bundle_id: Option<String>,
    /// If `true`, the window will be managed as a floating window (not tiled).
    pub floating: Option<bool>,
    /// An optional preferred index for the window's position in the window strip.
    pub index: Option<usize>,
    pub vertical_padding: Option<i32>,
    pub horizontal_padding: Option<i32>,
    pub dont_focus: Option<bool>,
    /// An optional initial width ratio (0.0–1.0) relative to the display width.
    /// Overrides the default column width when the window is first managed.
    pub width: Option<f64>,
}

/// Deserializes a regular expression from a string for window titles.
fn deserialize_title<'de, D>(deserializer: D) -> std::result::Result<Regex, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    Regex::new(&s).map_err(de::Error::custom)
}

/// Parses a string containing modifier names (e.g., "alt", "shift", "cmd", "ctrl") separated by "+", and returns their combined bitmask.
///
/// # Arguments
///
/// * `input` - The string containing modifier names (e.g., "ctrl+alt").
///
/// # Returns
///
/// `Ok(Modifiers)` with the combined modifier bitmask if parsing is successful, otherwise `Err(String)` with an error message for an invalid modifier.
fn parse_modifiers(input: &str) -> Result<Modifiers> {
    let mut out = Modifiers::empty();

    let modifiers = input.split('+').map(str::trim).collect::<Vec<_>>();
    for modifier in &modifiers {
        out |= match *modifier {
            "alt" => Modifiers::ALT,
            "shift" => Modifiers::SHIFT,
            "cmd" => Modifiers::CMD,
            "ctrl" => Modifiers::CTRL,
            _ => {
                return Err(Error::InvalidConfig(format!(
                    "{}: Invalid modifier: {modifier}",
                    function_name!()
                )));
            }
        }
    }
    Ok(out)
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    /// Returns a reference to the currently selected keyboard layout input source that is ASCII-capable.
    ///
    /// # Returns
    ///
    /// A raw pointer to the `TISInputSourceRef` (a `c_void` pointer) if successful, otherwise `null_mut()`.
    fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> *mut c_void;

    /// Retrieves a specified property of an input source.
    ///
    /// # Arguments
    ///
    /// * `keyboard` - The raw pointer to the `TISInputSourceRef`.
    /// * `property` - The `CFStringRef` representing the property to retrieve (e.g., `kTISPropertyUnicodeKeyLayoutData`).
    ///
    /// # Returns
    ///
    /// A raw pointer to `CFData` containing the property value.
    fn TISGetInputSourceProperty(keyboard: *const c_void, property: CFStringRef) -> *mut CFData;

    /// Translates a virtual key code to a Unicode string according to the specified keyboard layout.
    ///
    /// # Arguments
    ///
    /// * `keyLayoutPtr` - A pointer to the keyboard layout data.
    /// * `virtualKeyCode` - The virtual key code to translate.
    /// * `keyAction` - The key action (e.g., `UCKeyAction::Down`).
    /// * `modifierKeyState` - The state of the modifier keys (e.g., `kUCKeyModifierAlphaLockBit`).
    /// * `keyboardType` - The type of keyboard, typically obtained from `LMGetKbdType()`.
    /// * `keyTranslateOptions` - Options for the translation process.
    /// * `deadKeyState` - A mutable reference to a `u32` representing the dead key state.
    /// * `maxStringLength` - The maximum length of the output Unicode string buffer.
    /// * `actualStringLength` - A mutable reference to an `isize` to store the actual length of the output Unicode string.
    /// * `unicodeString` - A mutable pointer to the buffer to store the resulting Unicode string.
    ///
    /// # Returns
    ///
    /// An `OSStatus` indicating success or failure.
    fn UCKeyTranslate(
        keyLayoutPtr: *mut u8,
        virtualKeyCode: u16,
        keyAction: u16,
        modifierKeyState: u32,
        keyboardType: u32,
        keyTranslateOptions: u32,
        deadKeyState: &mut u32,
        maxStringLength: usize,
        actualStringLength: &mut isize,
        unicodeString: *mut u16,
    ) -> OSStatus;

    /// Returns the keyboard type for the system.
    ///
    /// # Returns
    ///
    /// A `u8` representing the keyboard type.
    fn LMGetKbdType() -> u8;

    /// A constant `CFStringRef` representing the property key for Unicode keyboard layout data.
    static kTISPropertyUnicodeKeyLayoutData: CFStringRef;

}

/// Returns an iterator over static tuples of virtual key names and their corresponding keycodes.
/// These keycodes identify physical keys on an ANSI-standard US keyboard layout.
///
/// # Returns
///
/// An iterator yielding references to `(&'static str, u8)` tuples.
fn virtual_keycode() -> impl Iterator<Item = &'static (&'static str, u8)> {
    /*
     *  Summary:
     *    Virtual keycodes
     *
     *  Discussion:
     *    These constants are the virtual keycodes defined originally in
     *    Inside Mac Volume V, pg. V-191. They identify physical keys on a
     *    keyboard. Those constants with "ANSI" in the name are labeled
     *    according to the key position on an ANSI-standard US keyboard.
     *    For example, kVK_ANSI_A indicates the virtual keycode for the key
     *    with the letter 'A' in the US keyboard layout. Other keyboard
     *    layouts may have the 'A' key label on a different physical key;
     *    in this case, pressing 'A' will generate a different virtual
     *    keycode.
     */
    static VIRTUAL_KEYCODE: LazyLock<Vec<(&'static str, u8)>> = LazyLock::new(|| {
        vec![
            ("a", 0x00),
            ("s", 0x01),
            ("d", 0x02),
            ("f", 0x03),
            ("h", 0x04),
            ("g", 0x05),
            ("z", 0x06),
            ("x", 0x07),
            ("c", 0x08),
            ("v", 0x09),
            ("section", 0x0a), // iso keyboards only.
            ("b", 0x0b),
            ("q", 0x0c),
            ("w", 0x0d),
            ("e", 0x0e),
            ("r", 0x0f),
            ("y", 0x10),
            ("t", 0x11),
            ("1", 0x12),
            ("2", 0x13),
            ("3", 0x14),
            ("4", 0x15),
            ("6", 0x16),
            ("5", 0x17),
            ("equal", 0x18),
            ("9", 0x19),
            ("7", 0x1a),
            ("minus", 0x1b),
            ("8", 0x1c),
            ("0", 0x1d),
            ("rightbracket", 0x1e),
            ("o", 0x1f),
            ("u", 0x20),
            ("leftbracket", 0x21),
            ("i", 0x22),
            ("p", 0x23),
            ("l", 0x25),
            ("j", 0x26),
            ("quote", 0x27),
            ("k", 0x28),
            ("semicolon", 0x29),
            ("backslash", 0x2a),
            ("comma", 0x2b),
            ("slash", 0x2c),
            ("n", 0x2d),
            ("m", 0x2e),
            ("period", 0x2f),
            ("grave", 0x32),
            ("keypaddecimal", 0x41),
            ("keypadmultiply", 0x43),
            ("keypadplus", 0x45),
            ("keypadclear", 0x47),
            ("keypaddivide", 0x4b),
            ("keypadenter", 0x4c),
            ("keypadminus", 0x4e),
            ("keypadequals", 0x51),
            ("keypad0", 0x52),
            ("keypad1", 0x53),
            ("keypad2", 0x54),
            ("keypad3", 0x55),
            ("keypad4", 0x56),
            ("keypad5", 0x57),
            ("keypad6", 0x58),
            ("keypad7", 0x59),
            ("keypad8", 0x5b),
            ("keypad9", 0x5c),
        ]
    });
    VIRTUAL_KEYCODE.iter()
}

/// Returns an iterator over static tuples of literal key names and their corresponding keycodes.
/// These keycodes are for keys that are independent of the keyboard layout (e.g., Return, Tab, Space).
///
/// # Returns
///
/// An iterator yielding references to `(&'static str, u8)` tuples.
fn literal_keycode() -> impl Iterator<Item = &'static (&'static str, u8)> {
    /* keycodes for keys that are independent of keyboard layout*/
    static LITERAL_KEYCODE: LazyLock<Vec<(&'static str, u8)>> = LazyLock::new(|| {
        vec![
            ("return", 0x24),
            ("tab", 0x30),
            ("space", 0x31),
            ("delete", 0x33),
            ("escape", 0x35),
            ("command", 0x37),
            ("shift", 0x38),
            ("capslock", 0x39),
            ("option", 0x3a),
            ("control", 0x3b),
            ("rightcommand", 0x36),
            ("rightshift", 0x3c),
            ("rightoption", 0x3d),
            ("rightcontrol", 0x3e),
            ("function", 0x3f),
            ("f17", 0x40),
            ("volumeup", 0x48),
            ("volumedown", 0x49),
            ("mute", 0x4a),
            ("f18", 0x4f),
            ("f19", 0x50),
            ("f20", 0x5a),
            ("f5", 0x60),
            ("f6", 0x61),
            ("f7", 0x62),
            ("f3", 0x63),
            ("f8", 0x64),
            ("f9", 0x65),
            ("f11", 0x67),
            ("f13", 0x69),
            ("f16", 0x6a),
            ("f14", 0x6b),
            ("f10", 0x6d),
            ("contextualmenu", 0x6e),
            ("f12", 0x6f),
            ("f15", 0x71),
            ("help", 0x72),
            ("home", 0x73),
            ("pageup", 0x74),
            ("forwarddelete", 0x75),
            ("f4", 0x76),
            ("end", 0x77),
            ("f2", 0x78),
            ("pagedown", 0x79),
            ("f1", 0x7a),
            ("leftarrow", 0x7b),
            ("rightarrow", 0x7c),
            ("downarrow", 0x7d),
            ("uparrow", 0x7e),
        ]
    });
    LITERAL_KEYCODE.iter()
}

/// Represents the action of a key, used in `UCKeyTranslate`.
enum UCKeyAction {
    /// The key is going down.
    Down = 0, // key is going down
              /*
              Up = 1,      // key is going up
              AutoKey = 2, // auto-key down
              Display = 3, // get information for key display (as in Key Caps)
              */
}

/// Generates a vector of (`key_name`, keycode) tuples for virtual keys based on the current ASCII-capable keyboard layout.
/// This involves using macOS Carbon API functions to translate virtual keycodes to Unicode characters.
///
/// # Returns
///
/// A `Vec<(String, u8)>` containing the translated key names and their keycodes. Returns an empty vector if an error occurs during keyboard layout fetching.
fn generate_virtual_keymap() -> Vec<(String, u8)> {
    let keyboard = AXUIWrapper::from_retained(unsafe {
        TISCopyCurrentASCIICapableKeyboardLayoutInputSource()
    })
    .ok();
    let keyboard_layout = keyboard
        .and_then(|keyboard| {
            NonNull::new(unsafe {
                TISGetInputSourceProperty(
                    keyboard.as_ptr::<c_void>(),
                    kTISPropertyUnicodeKeyLayoutData,
                )
            })
        })
        .and_then(|uchr| NonNull::new(unsafe { CFData::byte_ptr(uchr.as_ref()).cast_mut() }));
    let Some(keyboard_layout) = keyboard_layout else {
        error!(
            "{}: problem fetching current virtual keyboard layout.",
            function_name!()
        );
        return vec![];
    };

    let mut state = 0u32;
    let mut chars = vec![0u16; 256];
    let mut got: isize = 0;
    virtual_keycode()
        .filter_map(|(_, keycode)| {
            unsafe {
                UCKeyTranslate(
                    keyboard_layout.as_ptr(),
                    (*keycode).into(),
                    UCKeyAction::Down as u16,
                    0,
                    LMGetKbdType().into(),
                    1,
                    &mut state,
                    chars.len(),
                    &mut got,
                    chars.as_mut_ptr(),
                )
            }
            .to_result(function_name!())
            .ok()
            .map(|()| {
                let name = unsafe { CFString::with_characters(None, chars.as_ptr(), got) }
                    .map(|chars| chars.to_string());
                name.zip(Some(*keycode))
            })
        })
        .flatten()
        .collect()
}

#[test]
fn test_config_parsing() {
    let input = r#"
[options]
focus_follows_mouse = true

[bindings]
quit = "ctrl+alt-q"
window_manage = "ctrl+alt-t"
window_stack = ["ctrl-s", "alt-s"]

[windows]

[windows.pip]
title = "picture.*picture"
bundle_id = "com.something.apple"
floating = true
index = 1
"#;
    let config = Config {
        inner: Arc::new(ArcSwap::from_pointee(
            InnerConfig::parse_config(input).expect("Failed to parse config"),
        )),
    };
    let find_key = |k| {
        virtual_keycode()
            .find_map(|(s, v)| (format!("{k}") == *s).then_some(*v))
            .unwrap()
    };

    assert_eq!(config.inner().options.focus_follows_mouse, Some(true));

    // Modifiers: alt = 1<<0, ctrl = 1<<3.
    let keycode = find_key('q');
    assert!(matches!(
        config.find_keybind(keycode, &(Modifiers::ALT | Modifiers::CTRL)),
        Some(Command::Quit)
    ));

    let keycode = find_key('t');
    assert!(matches!(
        config.find_keybind(keycode, &(Modifiers::ALT | Modifiers::CTRL)),
        Some(Command::Window(Operation::Manage))
    ));

    let keycode = find_key('s');
    assert!(matches!(
        config.find_keybind(keycode, &Modifiers::CTRL),
        Some(Command::Window(Operation::Stack(true)))
    ));

    assert!(matches!(
        config.find_keybind(keycode, &Modifiers::ALT),
        Some(Command::Window(Operation::Stack(true)))
    ));

    let props = config.find_window_properties("picture in picture", "com.something.apple");
    assert_eq!(props[0].floating, Some(true));
    assert_eq!(props[0].index, Some(1));
}

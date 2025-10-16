use log::error;
use notify::EventHandler;
use objc2_core_foundation::{CFData, CFString};
use serde::{Deserialize, Deserializer, de};
use std::{
    collections::HashMap,
    ffi::c_void,
    path::Path,
    ptr::NonNull,
    sync::{Arc, LazyLock, RwLock},
};
use stdext::function_name;
use stdext::prelude::RwLockExt;

use crate::{platform::CFStringRef, skylight::OSStatus, util::AxuWrapperType};

#[derive(Clone, Debug)]
pub struct Config {
    inner: Arc<RwLock<InnerConfig>>,
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
    /// `Ok(Self)` if the configuration is loaded successfully, otherwise `Err(String)` with an error message.
    pub fn new(path: &Path) -> Result<Self, String> {
        Ok(Config {
            inner: RwLock::new(InnerConfig::new(path)?).into(),
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
    /// `Ok(())` if the configuration is reloaded successfully, otherwise `Err(String)` with an error message.
    pub fn reload_config(&mut self, path: &Path) -> Result<(), String> {
        let new = InnerConfig::new(path)?;
        let mut old = self.inner.force_write();
        old.options = new.options;
        old.bindings = new.bindings;
        Ok(())
    }

    /// Returns a read guard to the inner `InnerConfig` for read-only access.
    ///
    /// # Returns
    ///
    /// A `std::sync::RwLockReadGuard` allowing read access to `InnerConfig`.
    fn inner(&self) -> std::sync::RwLockReadGuard<'_, InnerConfig> {
        self.inner.force_read()
    }

    /// Returns a clone of the `MainOptions` from the current configuration.
    ///
    /// # Returns
    ///
    /// A `MainOptions` struct containing the main configuration options.
    pub fn options(&self) -> MainOptions {
        self.inner().options.clone()
    }

    /// Finds a keybinding matching the given keycode and modifier mask.
    ///
    /// # Arguments
    ///
    /// * `keycode` - The key code of the keybinding to find.
    /// * `mask` - The modifier mask (e.g., Alt, Shift, Cmd, Ctrl) of the keybinding.
    ///
    /// # Returns
    ///
    /// `Some(Keybinding)` if a matching keybinding is found, otherwise `None`.
    pub fn find_keybind(&self, keycode: u8, mask: u8) -> Option<Keybinding> {
        let lock = self.inner();
        lock.bindings
            .values()
            .find(|bind| bind.code == keycode && bind.modifiers == mask)
            .cloned()
    }
}

impl EventHandler for Config {
    /// Handles file system events, specifically used for reloading the configuration file.
    ///
    /// # Arguments
    ///
    /// * `event` - The result of a file system event.
    fn handle_event(&mut self, event: notify::Result<notify::Event>) {
        if let Ok(event) = event {
            println!("Event: {event:?}");
        }
    }
}

#[derive(Deserialize, Debug)]
struct InnerConfig {
    options: MainOptions,
    bindings: HashMap<String, Keybinding>,
}

impl InnerConfig {
    /// Creates a new `InnerConfig` by reading and parsing the configuration file from the specified path.
    ///
    /// # Arguments
    ///
    /// * `path` - A reference to the path of the configuration file.
    ///
    /// # Returns
    ///
    /// `Ok(InnerConfig)` if the configuration is parsed successfully, otherwise `Err(String)` with an error message.
    fn new(path: &Path) -> Result<InnerConfig, String> {
        let input = std::fs::read_to_string(path).map_err(|err| {
            format!(
                "{}: can't open configuration in ~/.paneru - {err}",
                function_name!()
            )
        })?;
        InnerConfig::parse_config(&input)
    }

    /// Parses the configuration from a string input.
    /// It populates the `code` and `command` fields of `Keybinding` by looking up virtual keys and literal keycodes.
    ///
    /// # Arguments
    ///
    /// * `input` - The string content of the configuration file.
    ///
    /// # Returns
    ///
    /// `Ok(InnerConfig)` if the parsing is successful, otherwise `Err(String)` with an error message.
    fn parse_config(input: &str) -> Result<InnerConfig, String> {
        let virtual_keys = generate_virtual_keymap();
        let mut config: InnerConfig = toml::from_str(input)
            .map_err(|err| format!("{}: error loading config: {err}", function_name!()))?;

        config.bindings.iter_mut().for_each(|(command, binding)| {
            binding.command = command.clone();
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
            }
            println!("bind: {binding:?}");
        });

        Ok(config)
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct MainOptions {
    pub focus_follows_mouse: bool,
}

#[derive(Clone, Debug)]
pub struct Keybinding {
    pub key: String,
    pub code: u8,
    pub modifiers: u8,
    pub command: String,
}

impl<'de> Deserialize<'de> for Keybinding {
    /// Deserializes a `Keybinding` from a string input. The input string is expected to be in a format like "modifier-key" or "key".
    ///
    /// # Arguments
    ///
    /// * `deserializer` - The deserializer used to parse the input.
    ///
    /// # Returns
    ///
    /// `Ok(Self)` if the deserialization is successful, otherwise `Err(D::Error)` with a custom error message.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let input = String::deserialize(deserializer)?;
        let mut parts = input.split("-").map(|s| s.trim()).collect::<Vec<_>>();
        let key = parts.pop();

        if parts.len() > 1 || key.is_none() {
            return Err(de::Error::custom(format!("Too many dashes: {input:?}")));
        }

        let modifiers = match parts.pop() {
            Some(modifiers) => parse_modifiers(modifiers).map_err(de::Error::custom)?,
            None => 0,
        };

        Ok(Keybinding {
            key: key.unwrap().to_string(),
            code: 0,
            modifiers,
            command: "".to_string(),
        })
    }
}

/// Parses a string containing modifier names (e.g., "alt", "shift", "cmd", "ctrl") separated by "+", and returns their combined bitmask.
///
/// # Arguments
///
/// * `input` - The string containing modifier names.
///
/// # Returns
///
/// `Ok(u8)` with the combined modifier bitmask if parsing is successful, otherwise `Err(String)` with an error message for an invalid modifier.
fn parse_modifiers(input: &str) -> Result<u8, String> {
    static MOD_NAMES: [&str; 4] = ["alt", "shift", "cmd", "ctrl"];
    let mut out = 0;

    let modifiers = input.split("+").map(|s| s.trim()).collect::<Vec<_>>();
    for modifier in modifiers.iter() {
        if !MOD_NAMES.iter().any(|name| name == modifier) {
            return Err(format!("Invalid modifier: {modifier}"));
        }

        if let Some((shift, _)) = MOD_NAMES
            .iter()
            .enumerate()
            .find(|(_, name)| *name == modifier)
        {
            out += 1 << shift;
        }
    }
    Ok(out)
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> *mut c_void;

    fn TISGetInputSourceProperty(keyboard: *const c_void, property: CFStringRef) -> *mut CFData;

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

    fn LMGetKbdType() -> u8;

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

enum UCKeyAction {
    Down = 0, // key is going down
              /*
              Up = 1,      // key is going up
              AutoKey = 2, // auto-key down
              Display = 3, // get information for key display (as in Key Caps)
              */
}

/// Generates a vector of (key_name, keycode) tuples for virtual keys based on the current ASCII-capable keyboard layout.
/// This involves using macOS Carbon API functions to translate virtual keycodes to Unicode characters.
///
/// # Returns
///
/// A `Vec<(String, u8)>` containing the translated key names and their keycodes. Returns an empty vector if an error occurs during keyboard layout fetching.
fn generate_virtual_keymap() -> Vec<(String, u8)> {
    let keyboard = AxuWrapperType::from_retained(unsafe {
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
        .and_then(|uchr| NonNull::new(unsafe { CFData::byte_ptr(uchr.as_ref()) as *mut u8 }));
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
        .flat_map(|(_, keycode)| unsafe {
            (0 == UCKeyTranslate(
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
            ))
            .then(|| {
                let name = CFString::with_characters(None, chars.as_ptr(), got)
                    .map(|chars| chars.to_string());
                name.zip(Some(*keycode))
            })
        })
        .flatten()
        .collect()
}

#[test]
fn test_config() {
    let input = r#"
# syntax
[options]
mouse_focus = true

[bindings]
single = "s"
simple = "alt-s"
quit = "ctrl + alt - q"
manage = "ctrl + alt - t"
"#;
    let config = InnerConfig::parse_config(input)
        .inspect_err(|err| println!("Error: {err}"))
        .unwrap();

    assert_eq!(format!("{:?}", config.bindings), "");
}

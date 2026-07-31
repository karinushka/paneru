//! Embedded Lua scripting runtime (mlua).
//!
//! The runtime lets a user's `init.lua` script hook into window-manager events
//! (`paneru.on`) and bind keys to Lua callbacks or command strings
//! (`paneru.bind`). Callbacks receive a read-only state snapshot and mutate the
//! window manager by issuing commands through `paneru.run`, which are funnelled
//! back onto the existing command bus.
//!
//! `mlua::Lua` is `!Send`, so [`LuaRuntime`] lives as a `NonSend` resource and is
//! only ever touched from the main-thread schedules. Every system takes it as
//! `Option<NonSendMut<LuaRuntime>>` so the mock test harness (which never inserts
//! a runtime) keeps compiling and the systems gracefully no-op.

mod api;
mod convert;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bevy::app::{App, Plugin, PreUpdate, Update};
use bevy::ecs::message::MessageReader;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, NonSendMut, Res};
use mlua::{Function, Lua, Table, Value};
use notify::Watcher;
use tracing::{error, info, warn};

use crate::commands::Command;
use crate::ecs::params::Windows;
use crate::ecs::{SendMessageTrigger, SpawnCommandsExt};
use crate::events::Event;
use crate::manager::WindowManager;
use crate::platform::Modifiers;
use crate::platform::input::set_lua_keybinds;
use crate::util::symlink_target;

/// A Lua-registered keybind: `(keycode, modifiers, handler_id)`.
pub(super) type LuaKeybind = (u8, Modifiers, u32);

/// Everything the script registered, kept on the Rust side so dispatch never
/// has to reach back into Lua globals to find a callback.
#[derive(Default)]
pub(super) struct Registry {
    /// `paneru.on` handlers, in registration order per event name.
    handlers: HashMap<String, Vec<Function>>,
    /// `paneru.bind` handlers indexed by `id - 1`: a Lua function, or a command
    /// string to run as-is.
    binds: Vec<Value>,
    /// Chords parallel to `binds`, for publishing to the event-tap registry.
    keybinds: Vec<LuaKeybind>,
}

/// Shared handle to the [`Registry`], captured by the registration closures and
/// read back when dispatching.
pub(super) type SharedRegistry = Rc<RefCell<Registry>>;

/// Pending side effects produced by Lua callbacks, drained after each dispatch.
#[derive(Default)]
pub(super) struct Outbox {
    /// Commands queued via `paneru.run` / `paneru.cmd`.
    commands: Vec<Command>,
    /// Flash messages queued via `paneru.flash` as `(message, duration_secs)`.
    flashes: Vec<(String, f32)>,
}

/// The Lua init-script path, kept as a resource so the reload system knows which
/// watched file to react to.
#[derive(Resource, Debug, Clone)]
pub struct LuaScriptPath(pub PathBuf);

/// The embedded Lua runtime and its shared registration state.
pub struct LuaRuntime {
    lua: Lua,
    outbox: Rc<RefCell<Outbox>>,
    registry: SharedRegistry,
}

impl LuaRuntime {
    /// Builds a runtime by reading and executing the script at `path`.
    pub fn from_file(path: &Path) -> mlua::Result<Self> {
        let source = std::fs::read_to_string(path).map_err(mlua::Error::external)?;
        Self::from_source(&source)
    }

    /// Builds a runtime from Lua source, installing the `paneru` API and
    /// executing the script. Registered keybinds are collected for publishing.
    pub fn from_source(source: &str) -> mlua::Result<Self> {
        let lua = Lua::new();
        let outbox = Rc::new(RefCell::new(Outbox::default()));
        let registry = SharedRegistry::default();
        api::install(&lua, &outbox, &registry)?;
        lua.load(source).exec()?;
        Ok(Self {
            lua,
            outbox,
            registry,
        })
    }

    fn lua(&self) -> &Lua {
        &self.lua
    }

    /// The `(keycode, modifiers, id)` keybinds registered by the script, for
    /// publishing to the event-tap registry.
    pub fn published_keybinds(&self) -> Vec<LuaKeybind> {
        self.registry.borrow().keybinds.clone()
    }

    /// Runs every `paneru.on(name, ...)` handler. Handlers are cloned out of the
    /// registry first so a handler that registers another one cannot deadlock on
    /// the borrow. One failing handler does not stop the rest.
    fn dispatch_event(&self, name: &str, event: &Table) {
        let handlers = self
            .registry
            .borrow()
            .handlers
            .get(name)
            .cloned()
            .unwrap_or_default();
        for handler in handlers {
            if let Err(err) = handler.call::<()>(event.clone()) {
                error!("lua event handler '{name}': {err}");
            }
        }
    }

    /// Runs the handler bound to keybind `id`: a Lua function gets the state
    /// snapshot, a command string goes straight onto the outbox.
    fn dispatch_bind(&self, id: u32, state: Table) {
        let handler = id
            .checked_sub(1)
            .and_then(|index| self.registry.borrow().binds.get(index as usize).cloned());
        match handler {
            Some(Value::Function(handler)) => {
                if let Err(err) = handler.call::<()>(state) {
                    error!("lua keybind handler {id}: {err}");
                }
            }
            Some(Value::String(command)) => {
                let command = command.to_string_lossy();
                let argv: Vec<&str> = command.split_whitespace().collect();
                match crate::config::parse_command(&argv) {
                    Ok(command) => self.outbox.borrow_mut().commands.push(command),
                    Err(err) => error!("lua keybind {id} command '{command}': {err}"),
                }
            }
            _ => warn!("lua keybind {id} has no handler"),
        }
    }

    /// Drains queued commands and flash messages onto the command bus.
    fn apply(&self, commands: &mut Commands) {
        let mut outbox = self.outbox.borrow_mut();
        for command in outbox.commands.drain(..) {
            commands.trigger(SendMessageTrigger(Event::Command { command }));
        }
        for (message, duration) in outbox.flashes.drain(..) {
            commands.flash_message(message, duration);
        }
    }

    /// An empty runtime (no handlers, no binds). Used as a fallback when the
    /// init script fails to load, so the resource always exists and a later
    /// hot reload can install a fixed script.
    pub fn empty() -> Self {
        Self::from_source("").expect("empty Lua runtime should always build")
    }
}

/// Registers the Lua runtime systems. Added only in the real app, not the mock
/// test harness.
pub struct LuaPlugin;

impl Plugin for LuaPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PreUpdate, command_lua_handler);
        app.add_systems(Update, (dispatch_lua_events, lua_reload_system));
    }
}

/// Forwards window-manager events to registered `paneru.on` callbacks.
pub fn dispatch_lua_events(
    runtime: Option<NonSendMut<LuaRuntime>>,
    mut reader: MessageReader<Event>,
    mut commands: Commands,
) {
    let Some(runtime) = runtime else {
        return;
    };
    let mut dispatched = false;
    for event in reader.read() {
        if let Some((name, table)) = convert::event_to_lua(runtime.lua(), event) {
            runtime.dispatch_event(name, &table);
            dispatched = true;
        }
    }
    if dispatched {
        runtime.apply(&mut commands);
    }
}

/// Handles `Command::Lua(id)` by invoking the bound Lua callback with a state
/// snapshot, then draining any commands it queued.
#[allow(clippy::needless_pass_by_value)]
pub fn command_lua_handler(
    runtime: Option<NonSendMut<LuaRuntime>>,
    mut reader: MessageReader<Event>,
    windows: Windows,
    mut commands: Commands,
) {
    let Some(runtime) = runtime else {
        return;
    };
    let ids: Vec<u32> = reader
        .read()
        .filter_map(|event| match event {
            Event::Command {
                command: Command::Lua(id),
            } => Some(*id),
            _ => None,
        })
        .collect();
    if ids.is_empty() {
        return;
    }
    let state = match convert::state_snapshot(runtime.lua(), &windows) {
        Ok(state) => state,
        Err(err) => {
            error!("lua state snapshot: {err}");
            return;
        }
    };
    for id in ids {
        runtime.dispatch_bind(id, state.clone());
    }
    runtime.apply(&mut commands);
}

/// Rebuilds the Lua runtime when the init script changes, committing atomically
/// only on success so a broken edit never tears down the working setup.
#[allow(clippy::needless_pass_by_value)]
pub fn lua_reload_system(
    mut runtime: Option<NonSendMut<LuaRuntime>>,
    script_path: Option<Res<LuaScriptPath>>,
    mut reader: MessageReader<Event>,
    window_manager: Res<WindowManager>,
    mut watcher: Option<NonSendMut<Box<dyn Watcher>>>,
    mut commands: Commands,
) {
    let (Some(runtime), Some(script_path)) = (runtime.as_mut(), script_path) else {
        return;
    };
    let path = &script_path.0;

    let mut should_reload = false;
    for event in reader.read() {
        let Event::ConfigRefresh(event) = event else {
            continue;
        };
        if event.paths.iter().any(|changed| paths_match(changed, path)) {
            // Editors that atomically replace files (write-new-then-rename)
            // break the original watch; re-establish it like the TOML handler.
            if let (Some(watcher), Some(symlink)) = (watcher.as_mut(), symlink_target(path))
                && let Ok(new_watcher) =
                    window_manager
                        .setup_config_watcher(path)
                        .inspect_err(|err| {
                            error!("re-watching lua script '{}': {err}", symlink.display());
                        })
            {
                **watcher = new_watcher;
            }
            should_reload = true;
        }
    }
    if !should_reload {
        return;
    }

    match LuaRuntime::from_file(path) {
        Ok(new_runtime) => {
            set_lua_keybinds(new_runtime.published_keybinds());
            **runtime = new_runtime;
            info!("Reloaded Lua script {}", path.display());
            commands.flash_message("Lua reloaded".to_string(), 1.5);
        }
        Err(err) => {
            error!("Reloading Lua script '{}': {err}", path.display());
            commands.flash_message(format!("Lua error: {err}"), 4.0);
        }
    }
}

/// Whether a change notification path refers to the watched script (directly or
/// by filename, covering atomic-save temp-file renames).
fn paths_match(changed: &Path, script: &Path) -> bool {
    changed == script || changed.file_name() == script.file_name()
}

/// Loads the runtime for `path`, falling back to an empty runtime (and logging)
/// if the script errors. Always publishes the resulting keybinds.
pub fn load_runtime(path: &Path) -> LuaRuntime {
    let runtime = match LuaRuntime::from_file(path) {
        Ok(runtime) => {
            info!("Loaded Lua script {}", path.display());
            runtime
        }
        Err(err) => {
            warn!("Loading Lua script '{}': {err}", path.display());
            LuaRuntime::empty()
        }
    };
    set_lua_keybinds(runtime.published_keybinds());
    runtime
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drains the outbox commands for assertions.
    fn drained_commands(runtime: &LuaRuntime) -> Vec<Command> {
        runtime.outbox.borrow_mut().commands.drain(..).collect()
    }

    #[test]
    fn bind_registers_keybind_and_stores_handler() {
        let runtime =
            LuaRuntime::from_source(r#"paneru.bind("alt - j", "window focus east")"#).unwrap();
        let binds = runtime.published_keybinds();
        assert_eq!(binds.len(), 1);
        let (_, modifiers, id) = binds[0];
        assert_eq!(modifiers, Modifiers::ALT);
        assert_eq!(id, 1);
    }

    #[test]
    fn string_keybind_dispatch_queues_command() {
        let runtime =
            LuaRuntime::from_source(r#"paneru.bind("alt - b", "window balance")"#).unwrap();
        let state = runtime.lua().create_table().unwrap();
        runtime.dispatch_bind(1, state);
        let commands = drained_commands(&runtime);
        assert!(
            matches!(
                commands.as_slice(),
                [Command::Window(crate::commands::Operation::Balance)]
            ),
            "expected a balance command, got {commands:?}"
        );
    }

    #[test]
    fn function_keybind_can_run_commands() {
        let runtime = LuaRuntime::from_source(
            r#"paneru.bind("alt - j", function(state) paneru.run("window focus east") end)"#,
        )
        .unwrap();
        let state = runtime.lua().create_table().unwrap();
        runtime.dispatch_bind(1, state);
        assert_eq!(drained_commands(&runtime).len(), 1);
    }

    #[test]
    fn event_handler_receives_event_and_queues_command() {
        let runtime = LuaRuntime::from_source(
            r#"paneru.on("space_changed", function(e) paneru.run("window balance") end)"#,
        )
        .unwrap();
        let (name, table) = convert::event_to_lua(runtime.lua(), &Event::SpaceChanged).unwrap();
        runtime.dispatch_event(name, &table);
        assert_eq!(drained_commands(&runtime).len(), 1);
    }

    #[test]
    fn invalid_command_string_is_reported_not_panicking() {
        // A bad command string surfaces as a Lua runtime error at bind time.
        let result = LuaRuntime::from_source(r#"paneru.run("definitely not a command")"#);
        assert!(result.is_err());
    }

    #[test]
    fn empty_runtime_has_no_binds() {
        let runtime = LuaRuntime::empty();
        assert!(runtime.published_keybinds().is_empty());
    }
}

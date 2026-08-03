//! Embedded Lua scripting runtime (mlua).
//!
//! The runtime lets a user's `init.lua` script hook into window-manager events
//! (`paneru.on`) and bind keys to Lua callbacks or command strings
//! (`paneru.bind`). Callbacks receive a read-only state snapshot, can read the
//! full query documents through `paneru.query*` (see [`LuaRuntime::with_query`]),
//! and mutate the window manager by issuing commands through `paneru.run`, which
//! are funnelled back onto the existing command bus.
//!
//! This module is the ECS half: the systems that collect events out of the
//! world, hand them to the interpreter, and put what the callbacks queued back
//! onto the command bus. The interpreter itself lives in [`runtime`] and knows
//! nothing about Bevy — it reaches the world only through the `extract`
//! callback these systems supply.
//!
//! `mlua::Lua` is `!Send`, so [`LuaRuntime`] lives as a `NonSend` resource and is
//! only ever touched from the main-thread schedules. Every system takes it as
//! `Option<NonSendMut<LuaRuntime>>` so the mock test harness (which never inserts
//! a runtime) keeps compiling and the systems gracefully no-op.

mod api;
mod convert;
mod runtime;

use std::path::{Path, PathBuf};

use bevy::app::{App, Plugin, PreUpdate, Update};
use bevy::ecs::message::MessageReader;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, NonSendMut, Res};
use mlua::Table;
use notify::Watcher;
use tracing::{error, info, warn};

use crate::commands::Command;
use crate::ecs::state::QueryStateParams;
use crate::ecs::{SendMessageTrigger, SpawnCommandsExt};
use crate::events::Event;
use crate::manager::WindowManager;
use crate::platform::input::set_lua_keybinds;
use crate::util::symlink_target;

pub use runtime::LuaRuntime;
use runtime::{Outbox, SharedRegistry};

/// The Lua init-script path, kept as a resource so the reload system knows which
/// watched file to react to.
#[derive(Resource, Debug, Clone)]
pub struct LuaScriptPath(pub PathBuf);

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
#[allow(clippy::needless_pass_by_value)]
pub fn dispatch_lua_events(
    runtime: Option<NonSendMut<LuaRuntime>>,
    mut reader: MessageReader<Event>,
    state: QueryStateParams,
    mut commands: Commands,
) {
    let Some(runtime) = runtime else {
        return;
    };
    let runtime = &*runtime;
    // No `paneru.on` handlers means no consumer for any of these events, so skip
    // marshalling them into Lua tables entirely — just advance past them.
    if !runtime.has_event_handlers() {
        for _ in reader.read() {}
        return;
    }
    // Marshalled up front: the events cannot be read while the dispatch scope
    // below borrows the world for `paneru.query`.
    let events: Vec<(String, Table)> = reader
        .read()
        .filter_map(|event| convert::LuaEvent::try_from(event).ok())
        .filter_map(|event| convert::event_table(runtime.lua(), &event))
        .collect();
    if events.is_empty() {
        return;
    }
    let effects = runtime.dispatch_with_query("event dispatch", &extractor(&state), || {
        for (name, table) in &events {
            runtime.dispatch_event(name, table);
        }
    });
    apply(effects, &mut commands);
}

/// Handles `Command::Lua(id)` by invoking the bound Lua callback with a state
/// snapshot, then draining any commands it queued.
#[allow(clippy::needless_pass_by_value)]
pub fn command_lua_handler(
    runtime: Option<NonSendMut<LuaRuntime>>,
    mut reader: MessageReader<Event>,
    state: QueryStateParams,
    mut commands: Commands,
) {
    let Some(runtime) = runtime else {
        return;
    };
    let runtime = &*runtime;
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
    let snapshot = convert::state_snapshot(state.windows());
    let snapshot = match convert::snapshot_table(runtime.lua(), &snapshot) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            error!("lua state snapshot: {err}");
            return;
        }
    };
    let effects = runtime.dispatch_with_query("keybind dispatch", &extractor(&state), || {
        for id in ids {
            runtime.dispatch_bind(id, snapshot.clone());
        }
    });
    apply(effects, &mut commands);
}

/// The world-reading half of `paneru.query*`, as the plain callback the runtime
/// takes. Keeping it a closure over the system param is what stops the runtime
/// from needing to know about the ECS at all.
fn extractor<'a>(
    state: &'a QueryStateParams<'_, '_>,
) -> impl Fn() -> Result<crate::ecs::state::PaneruQueryState, String> + 'a {
    || state.extract().map_err(|err| err.to_string())
}

/// Puts the side effects of a dispatch onto the command bus.
fn apply(effects: runtime::Effects, commands: &mut Commands) {
    let (queued, flashes) = effects;
    for command in queued {
        commands.trigger(SendMessageTrigger(Event::Command { command }));
    }
    for (message, duration) in flashes {
        commands.flash_message(message, duration);
    }
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

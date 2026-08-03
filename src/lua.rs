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
mod worker;

use std::path::{Path, PathBuf};

use bevy::app::{App, Plugin, PostUpdate, PreUpdate, Update};
use bevy::ecs::message::MessageReader;
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::system::{Commands, NonSendMut, Res};
use notify::Watcher;
use tracing::error;

use crate::commands::Command;
use crate::ecs::params::Windows;
use crate::ecs::state::QueryStateParams;
use crate::ecs::{SendMessageTrigger, SpawnCommandsExt};
use crate::events::Event;
use crate::manager::WindowManager;
use crate::util::symlink_target;

use worker::FromLua;
pub use worker::{LuaSource, LuaWorker};

/// The Lua init-script path, kept as a resource so the reload system knows which
/// watched file to react to.
#[derive(Resource, Debug, Clone)]
pub struct LuaScriptPath(pub PathBuf);

/// Registers the Lua runtime systems. Added only in the real app, not the mock
/// test harness.
pub struct LuaPlugin;

impl Plugin for LuaPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PreUpdate,
            (
                // Before the pump so a query left outstanding from last frame is
                // answered before the main thread goes back to sleep waiting on
                // Cocoa.
                serve_lua_queries.before(crate::ecs::systems::pump_events),
                drain_lua_outbox,
                command_lua_handler,
            ),
        );
        // ...and again after everything, for queries a handler made during this
        // frame's `Update` dispatch.
        app.add_systems(PostUpdate, serve_lua_queries);
        app.add_systems(Update, (dispatch_lua_events, lua_reload_system));
    }
}

/// Forwards window-manager events to the worker for dispatch to `paneru.on`
/// callbacks.
#[allow(clippy::needless_pass_by_value)]
pub fn dispatch_lua_events(worker: Option<Res<LuaWorker>>, mut reader: MessageReader<Event>) {
    let Some(worker) = worker else {
        return;
    };
    // No `paneru.on` handlers means no consumer for any of these events, so skip
    // extracting them entirely — just advance past them.
    if !worker.has_event_handlers() {
        for _ in reader.read() {}
        return;
    }
    let events: Vec<convert::LuaEvent> = reader
        .read()
        .filter_map(|event| convert::LuaEvent::try_from(event).ok())
        .collect();
    if events.is_empty() {
        return;
    }
    worker.send_events(events);
}

/// Handles `Command::Lua(id)` by handing the bound callback and a state
/// snapshot to the worker.
#[allow(clippy::needless_pass_by_value)]
pub fn command_lua_handler(
    worker: Option<Res<LuaWorker>>,
    mut reader: MessageReader<Event>,
    windows: Windows,
) {
    let Some(worker) = worker else {
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
    worker.send_binds(ids, convert::state_snapshot(&windows));
}

/// Answers the `paneru.query*` calls waiting on the world.
///
/// This is the whole of the worker's world access: it asks over a channel and
/// blocks on the reply while the main thread carries on, so a handler mid-query
/// costs a frame of its own latency and none of anyone else's. Run in both
/// `PreUpdate` and `PostUpdate`; on an empty queue it costs one `try_recv`.
#[allow(clippy::needless_pass_by_value)]
pub fn serve_lua_queries(worker: Option<Res<LuaWorker>>, state: QueryStateParams) {
    let Some(worker) = worker else {
        return;
    };
    for request in worker.pending_queries() {
        request.answer(state.extract().map_err(|err| err.to_string()));
    }
}

/// Puts what the callbacks queued onto the command bus.
#[allow(clippy::needless_pass_by_value)]
pub fn drain_lua_outbox(worker: Option<Res<LuaWorker>>, mut commands: Commands) {
    let Some(worker) = worker else {
        return;
    };
    for effect in worker.drain_outbox() {
        match effect {
            FromLua::Command(command) => {
                commands.trigger(SendMessageTrigger(Event::Command { command }));
            }
            FromLua::Flash { message, duration } => commands.flash_message(message, duration),
        }
    }
}

/// Rebuilds the Lua runtime when the init script changes, committing atomically
/// only on success so a broken edit never tears down the working setup.
#[allow(clippy::needless_pass_by_value)]
pub fn lua_reload_system(
    worker: Option<Res<LuaWorker>>,
    script_path: Option<Res<LuaScriptPath>>,
    mut reader: MessageReader<Event>,
    window_manager: Res<WindowManager>,
    mut watcher: Option<NonSendMut<Box<dyn Watcher>>>,
) {
    let (Some(worker), Some(script_path)) = (worker, script_path) else {
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

    // The rebuild itself happens on the worker, which owns the interpreter:
    // it commits only on success, and reports either way through the outbox.
    worker.send_reload(path.clone());
}

/// Whether a change notification path refers to the watched script (directly or
/// by filename, covering atomic-save temp-file renames).
fn paths_match(changed: &Path, script: &Path) -> bool {
    changed == script || changed.file_name() == script.file_name()
}

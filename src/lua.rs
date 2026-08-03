//! Embedded Lua scripting runtime (mlua).
//!
//! The runtime lets a user's `init.lua` script hook into window-manager events
//! (`paneru.on`) and bind keys to Lua callbacks or command strings
//! (`paneru.bind`). Callbacks receive a read-only state snapshot, can read the
//! full query documents through `paneru.query*` (see [`LuaRuntime::with_query`]),
//! and mutate the window manager by issuing commands through `paneru.run`, which
//! are funnelled back onto the existing command bus.
//!
//! The interpreter does not run here. It runs on a thread of its own (see
//! [`worker`]), because a handler is arbitrary user code of unbounded duration
//! and the main thread is where the Cocoa event pump lives — a script that
//! takes a second used to freeze dragging, focus tracking and the menubar for
//! that second. This module is the ECS half either side of that channel: the
//! systems that collect events out of the world and hand them over, answer the
//! `paneru.query*` round-trip from the live world, and put what the callbacks
//! queued back onto the command bus.
//!
//! Three consequences worth knowing:
//!
//! * A handler that calls `paneru.query*` sees data up to about a frame stale,
//!   and waits about that long for it. The query documents are still read only
//!   on demand, so a script that never queries never pays for one; the window
//!   set is read once per batch whether or not a handler touches it, which is
//!   the cheaper of the two reads and no longer worth deferring.
//! * Commands a handler issues reach the command handlers one frame later than
//!   they did when dispatch was synchronous.
//! * Handlers within a batch run concurrently, so two of them reacting to the
//!   same event interleave at their world reads, and the order their commands
//!   reach the bus follows completion rather than registration. Commands from
//!   any *one* handler stay in the order it queued them.
//!
//! Every system takes the worker as `Option<Res<LuaWorker>>` so the mock test
//! harness (which never starts one) keeps compiling and the systems gracefully
//! no-op.

mod api;
mod convert;
mod runtime;
mod windowset;
mod worker;
mod world;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bevy::app::{App, Plugin, PostUpdate, PreUpdate, Update};
use bevy::ecs::message::MessageReader;
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::system::{Commands, NonSendMut, Query, Res, ResMut};
use notify::Watcher;

use crate::commands::Command;
use crate::config::Config;
use crate::ecs::params::Windows;
use crate::ecs::script_state::ScriptStateStore;
use crate::ecs::state::QueryStateParams;
use crate::ecs::{SendMessageTrigger, SpawnCommandsExt, apply_config_side_effects};
use crate::events::Event;
use crate::manager::{Application, Display, WindowManager};
use crate::util::symlink_target;

use worker::FromLua;
pub use worker::{LuaSource, LuaWorker};

/// The Lua init-script path, kept as a resource so the reload system knows which
/// watched file to react to.
#[derive(Resource, Debug, Clone)]
pub struct LuaScriptPath(pub PathBuf);

/// What a `paneru.state` call is told when there is no store in the world at
/// all. Only reachable in a harness that never inserted one.
const MISSING_STORE: &str = "the script state store is not available";

/// Registers the Lua runtime systems. Added only in the real app, not the mock
/// test harness.
pub struct LuaPlugin {}

impl Plugin for LuaPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PreUpdate,
            (
                // Before the pump so a read left outstanding from last frame is
                // answered before the main thread goes back to sleep waiting on
                // Cocoa. Two systems rather than one so the store's exclusive
                // access is not taken on behalf of a plain query — see
                // `serve_lua_queries`.
                serve_lua_queries.before(crate::ecs::systems::pump_events),
                serve_lua_store.before(crate::ecs::systems::pump_events),
                drain_lua_outbox,
                command_lua_handler,
            ),
        );
        // ...and again after everything, for reads a handler made during this
        // frame's `Update` dispatch.
        app.add_systems(PostUpdate, (serve_lua_queries, serve_lua_store));
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

/// Handles `Command::Lua(id)` by handing the bound callback to the worker.
#[allow(clippy::needless_pass_by_value)]
pub fn command_lua_handler(worker: Option<Res<LuaWorker>>, mut reader: MessageReader<Event>) {
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
    worker.send_binds(ids);
}

/// Answers the `paneru.query*` and window-set reads waiting on the world.
///
/// The worker asks over a channel and *awaits* the reply while the main thread
/// carries on, so a handler mid-query costs a frame of its own latency and none
/// of anyone else's. Run in both `PreUpdate` and `PostUpdate`; on an empty queue
/// it costs one `try_recv`.
///
/// The queue is drained before anything is extracted, and each kind is
/// extracted at most once per pass however many waiters asked for it. That
/// matters because extraction is not cheap — building the query document reads
/// every window's title over the accessibility API, one cross-process call
/// apiece — and because several handlers waiting at once is the normal case now
/// that dispatches overlap: they are all reading the same frame's world, so they
/// can all have the same answer.
///
/// Deliberately does *not* take the script state store. Bevy derives a system's
/// access from its parameters, statically and for the whole run, so asking for
/// the store here would make every pass hold it exclusively — blocking anything
/// else that touches it even when no script has mentioned `paneru.state`. That
/// half is [`serve_lua_store`]'s.
#[allow(clippy::needless_pass_by_value)]
pub fn serve_lua_queries(worker: Option<Res<LuaWorker>>, state: QueryStateParams) {
    let Some(worker) = worker else {
        return;
    };
    // Collected up front: extracting inside the loop would mean re-reading the
    // world for a waiter already sitting in the queue behind this one.
    let requests: Vec<_> = worker.pending_world_queries().collect();
    if requests.is_empty() {
        return;
    }

    // Filled on the first waiter that asks for that kind, reused by the rest.
    let mut extracted_state = None;
    let mut extracted_set = None;

    for request in requests {
        match request {
            worker::WorldRequest::State { reply } => {
                let _ = reply.try_send(extract_once(&mut extracted_state, || state.extract()));
            }
            worker::WorldRequest::WindowSet { reply } => {
                let _ = reply.try_send(extract_once(&mut extracted_set, || {
                    state.extract_window_set()
                }));
            }
        }
    }
}

/// Answers the `paneru.state` calls waiting on the store.
///
/// Separate from [`serve_lua_queries`] for the reason given there, and the
/// separation is worth more in this direction: this one needs the store
/// *mutably*, which is the widest access either half asks for. Kept to its own
/// system, that exclusivity lasts only for the store traffic and never bars the
/// systems that move windows around.
#[allow(clippy::needless_pass_by_value)]
pub fn serve_lua_store(
    worker: Option<Res<LuaWorker>>,
    mut script_state: Option<ResMut<ScriptStateStore>>,
) {
    let Some(worker) = worker else {
        return;
    };
    for request in worker.pending_store_queries() {
        match request {
            worker::StoreRequest::Read { reply } => {
                let answer = script_state
                    .as_ref()
                    .map(|store| store.snapshot())
                    .ok_or_else(|| MISSING_STORE.to_string());
                let _ = reply.try_send(answer);
            }
            // A write, unlike everything else here, is a handler waiting on an
            // answer it will act on: `paneru.state.mutate` retries when this
            // says the value was overtaken.
            worker::StoreRequest::Write { write, reply } => {
                let answer = script_state.as_mut().map_or_else(
                    || Err(MISSING_STORE.to_string()),
                    |store| store.apply(&write),
                );
                let _ = reply.try_send(answer);
            }
        }
    }
}

/// Reads the world once per pass, however many waiters ask for it.
///
/// `slot` holds what the first asker got — including a failure, which is shared
/// on the same terms rather than being retried per waiter.
fn extract_once<T>(
    slot: &mut Option<worker::Shared<T>>,
    extract: impl FnOnce() -> crate::errors::Result<T>,
) -> worker::Shared<T> {
    slot.get_or_insert_with(|| extract().map(Arc::new).map_err(|err| err.to_string()))
        .clone()
}

/// Puts what the callbacks queued onto the command bus.
///
/// Also the landing point for a reloaded `paneru.setup{...}`: the rebuild
/// happens on the worker, so the config it produced arrives here rather than in
/// [`lua_reload_system`], and this is where it is swapped into the shared handle.
#[allow(clippy::needless_pass_by_value)]
pub fn drain_lua_outbox(
    worker: Option<Res<LuaWorker>>,
    config: Option<Res<Config>>,
    mut displays: Query<&mut Display>,
    windows: Windows,
    applications: Query<&Application>,
    mut commands: Commands,
) {
    let Some(worker) = worker else {
        return;
    };
    for effect in worker.drain_outbox() {
        match effect {
            FromLua::Command(command) => {
                commands.trigger(SendMessageTrigger(Event::Command { command }));
            }
            FromLua::Flash { message, duration } => commands.flash_message(message, duration),
            FromLua::ConfigChanged => {
                // Swap the rebuilt settings into the handle every reader already
                // holds, then re-apply the same menubar/passthrough side effects
                // a TOML reload does.
                if let (Some(config), Some(built)) = (config.as_ref(), worker.built_config()) {
                    config.replace_inner_from(&built);
                    apply_config_side_effects(config, &mut displays, &windows, &applications);
                }
            }
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
            if let (Some(watcher), Some(_symlink)) = (watcher.as_mut(), symlink_target(path))
                && let Some(new_watcher) = crate::ecs::rewatch_configs(&window_manager, path)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// The point of the whole thing: many waiters, one read of the world.
    ///
    /// Before dispatches could overlap this was unobservable — each handler was
    /// in a frame of its own, so each legitimately re-read. Now that several can
    /// be queued at once, re-reading per waiter would multiply the accessibility
    /// traffic by however many handlers happened to be in flight.
    #[test]
    fn one_extraction_answers_every_waiter() {
        let reads = Cell::new(0);
        let mut slot = None;
        let answers: Vec<_> = (0..5)
            .map(|_| {
                extract_once(&mut slot, || {
                    reads.set(reads.get() + 1);
                    Ok(7_u32)
                })
            })
            .collect();

        assert_eq!(reads.get(), 1, "the world should be read once for all five");
        for answer in &answers {
            assert_eq!(*answer.as_ref().expect("a successful read"), Arc::new(7));
        }
        // ...and they share it rather than each holding a copy.
        let first = answers[0].as_ref().expect("a successful read");
        assert!(
            answers[1..]
                .iter()
                .all(|other| Arc::ptr_eq(first, other.as_ref().expect("a successful read"))),
            "every waiter should hold the same extraction"
        );
    }

    /// A world that could not be read is not re-read for the next waiter: the
    /// failure is shared on the same terms a success is. Retrying per waiter
    /// would turn one bad frame into one bad frame per handler.
    #[test]
    fn a_failed_extraction_is_shared_not_retried() {
        let reads = Cell::new(0);
        let mut slot = None;
        let answers: Vec<_> = (0..3)
            .map(|_| {
                extract_once(&mut slot, || -> crate::errors::Result<u32> {
                    reads.set(reads.get() + 1);
                    Err(crate::errors::Error::InvalidInput("no world".to_string()))
                })
            })
            .collect();

        assert_eq!(reads.get(), 1, "a failure should not be retried per waiter");
        assert!(answers.iter().all(std::result::Result::is_err));
    }
}

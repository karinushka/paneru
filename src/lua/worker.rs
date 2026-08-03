//! Runs the Lua interpreter on a thread of its own.
//!
//! A `paneru.on` handler or a keybind callback is arbitrary user code of
//! unbounded duration. On the main thread that is a hazard rather than a
//! nuisance: `pump_events` — the Cocoa event pump, and with it the frame clock —
//! is itself main-thread-pinned, so a handler that takes a second freezes window
//! dragging, focus tracking and the menubar for that second. `mlua::Lua` being
//! `!Send` is what forced it there; a dedicated thread with channels either side
//! is what gets it out.
//!
//! The shape:
//!
//! * The main thread only ever *sends* ([`ToLua`]) and *drains* ([`FromLua`]).
//!   Both channels are unbounded and every receive is non-blocking, so no
//!   scheduled system can ever wait on a script.
//! * `paneru.query*` still reads the live world, through a round-trip: the
//!   worker sends a [`QueryRequest`] carrying a reply channel and blocks on it
//!   while the main thread carries on, and `serve_lua_queries` answers it from
//!   the ECS on the next system that runs. That costs a handler which queries a
//!   frame of latency, and is why extraction stays lazy — a script that never
//!   queries never pays it.
//! * Because the runtime is `!Send` it cannot be *moved* here: [`spawn`] hands
//!   the thread a [`LuaSource`] and it builds the interpreter in place.
//!
//! Nothing crossing either channel is a Lua value; it is all plain data
//! ([`LuaEvent`], [`StateSnapshot`], [`Command`], [`PaneruQueryState`]), which
//! is what the marshalling split in [`super::convert`] exists to make possible.
//!
//! [`spawn`]: LuaWorker::spawn

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use bevy::ecs::resource::Resource;
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use tracing::{error, info, warn};

use super::convert::{self, LuaEvent, StateSnapshot};
use super::runtime::LuaRuntime;
use crate::commands::Command;
use crate::ecs::state::PaneruQueryState;
use crate::platform::input::set_lua_keybinds;

/// What the worker reported when the main thread has already gone away. Surfaces
/// inside the handler as an ordinary `paneru.query` error, so the script unwinds
/// normally instead of the thread hanging on a reply that can never come.
const SHUTTING_DOWN: &str = "the window manager is shutting down";

/// How long [`Drop`] waits for a dispatch in flight to finish before giving up
/// and detaching the thread. Enough for a well-behaved exit handler; bounded so
/// a script stuck in a loop can never stop the process from exiting.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);
const SHUTDOWN_POLL: Duration = Duration::from_millis(2);

/// Where a runtime's script comes from. The worker builds the interpreter
/// itself, so it needs the source rather than the built runtime.
pub enum LuaSource {
    Path(PathBuf),
    /// Source given directly. Only used by tests, which have no file to point at.
    #[cfg(test)]
    Inline(String),
}

/// Work for the interpreter. Unbounded and FIFO, so a reload can never overtake
/// events queued before it.
enum ToLua {
    /// One frame's worth of events, already extracted from the world.
    Events(Vec<LuaEvent>),
    /// One frame's worth of keybind ids, with the snapshot to hand them.
    Binds {
        ids: Vec<u32>,
        snapshot: StateSnapshot,
    },
    Reload(PathBuf),
    Shutdown,
}

/// A side effect a callback produced, on its way back to the command bus.
pub(super) enum FromLua {
    Command(Command),
    Flash { message: String, duration: f32 },
}

/// A `paneru.query*` call waiting on the world. Carries only the reply channel:
/// which kind was asked for, and whether as JSON, stays on the worker.
pub(super) struct QueryRequest {
    reply: Sender<Result<PaneruQueryState, String>>,
}

impl QueryRequest {
    /// Answers the waiting handler. Fails silently if it has already gone away.
    pub(super) fn answer(self, state: Result<PaneruQueryState, String>) {
        let _ = self.reply.send(state);
    }
}

/// The main thread's handle on the interpreter.
///
/// Unlike the runtime it stands for, this is `Send + Sync`: with crossbeam both
/// receiver ends are `Sync`, so every system can take it as `Res<LuaWorker>`
/// rather than `ResMut`, and none of them contend with each other.
#[derive(Resource)]
pub struct LuaWorker {
    to_lua: Sender<ToLua>,
    outbox: Receiver<FromLua>,
    queries: Receiver<QueryRequest>,
    /// Mirrors the runtime's `has_event_handlers`, republished after every load
    /// and reload. Lets the main thread keep its "no handlers, no marshalling"
    /// fast path without asking the worker and waiting for an answer.
    has_handlers: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LuaWorker {
    /// Starts the worker and waits for it to finish loading `source`.
    ///
    /// The wait is deliberate. This runs before `App::run`, where blocking costs
    /// nothing, and it keeps three properties the synchronous loader had: a
    /// script error is reported during startup rather than whenever the first
    /// event happens to arrive, keybinds are published before the event tap can
    /// see a keypress, and a broken script still leaves a working (empty)
    /// runtime behind.
    pub fn spawn(source: LuaSource) -> Self {
        let (to_lua, from_main) = unbounded();
        let (to_main, outbox) = unbounded();
        let (query_tx, queries) = unbounded();
        let (ready_tx, ready) = bounded(0);
        let has_handlers = Arc::new(AtomicBool::new(false));

        let thread = {
            let has_handlers = Arc::clone(&has_handlers);
            std::thread::Builder::new()
                .name("paneru-lua".to_string())
                .spawn(move || run(source, &from_main, &to_main, &query_tx, &has_handlers, &ready_tx))
                .expect("spawning the Lua worker thread")
        };
        // An error here means the thread died before finishing the load, which
        // `run` only does after logging why.
        let _ = ready.recv();

        Self {
            to_lua,
            outbox,
            queries,
            has_handlers,
            thread: Some(thread),
        }
    }

    /// Whether the loaded script registered any `paneru.on` handler.
    pub(super) fn has_event_handlers(&self) -> bool {
        self.has_handlers.load(Ordering::Relaxed)
    }

    /// Queues events for dispatch. Never blocks; a send only fails once the
    /// worker is gone, at which point there is nothing useful left to do.
    pub(super) fn send_events(&self, events: Vec<LuaEvent>) {
        let _ = self.to_lua.send(ToLua::Events(events));
    }

    /// Queues keybind callbacks for dispatch.
    pub(super) fn send_binds(&self, ids: Vec<u32>, snapshot: StateSnapshot) {
        let _ = self.to_lua.send(ToLua::Binds { ids, snapshot });
    }

    /// Asks the worker to rebuild itself from `path`.
    pub(super) fn send_reload(&self, path: PathBuf) {
        let _ = self.to_lua.send(ToLua::Reload(path));
    }

    /// The side effects callbacks have produced since the last drain.
    pub(super) fn drain_outbox(&self) -> impl Iterator<Item = FromLua> + '_ {
        self.outbox.try_iter()
    }

    /// The `paneru.query*` calls currently waiting on the world.
    pub(super) fn pending_queries(&self) -> impl Iterator<Item = QueryRequest> + '_ {
        self.queries.try_iter()
    }
}

impl Drop for LuaWorker {
    fn drop(&mut self) {
        let _ = self.to_lua.send(ToLua::Shutdown);
        let Some(thread) = self.thread.take() else {
            return;
        };
        // Dropping the query receiver is what unblocks a handler waiting on a
        // reply: its sender dies with the queue, so `recv` errors rather than
        // waiting forever. Give the dispatch in flight a moment to unwind.
        let mut waited = Duration::ZERO;
        while !thread.is_finished() && waited < SHUTDOWN_GRACE {
            std::thread::sleep(SHUTDOWN_POLL);
            waited += SHUTDOWN_POLL;
        }
        if thread.is_finished() {
            let _ = thread.join();
        } else {
            warn!("Lua worker did not stop in time; detaching it");
        }
    }
}

/// Builds the runtime for `source`, falling back to an empty one (and logging)
/// if the script errors, so a later hot reload can still install a fixed script.
/// Always publishes the resulting keybinds — [`set_lua_keybinds`] hands them to
/// the event tap through an `ArcSwap`, which is happy to be written from here.
fn load(source: &LuaSource) -> LuaRuntime {
    let runtime = match source {
        LuaSource::Path(path) => match LuaRuntime::from_file(path) {
            Ok(runtime) => {
                info!("Loaded Lua script {}", path.display());
                runtime
            }
            Err(err) => {
                warn!("Loading Lua script '{}': {err}", path.display());
                LuaRuntime::empty()
            }
        },
        #[cfg(test)]
        LuaSource::Inline(source) => LuaRuntime::from_source(source).unwrap_or_else(|err| {
            warn!("Loading inline Lua source: {err}");
            LuaRuntime::empty()
        }),
    };
    set_lua_keybinds(runtime.published_keybinds());
    runtime
}

/// Rebuilds the runtime from `path`, committing only on success so a broken
/// edit never tears down the working setup.
fn reload(runtime: &mut LuaRuntime, path: &Path, to_main: &Sender<FromLua>) {
    match LuaRuntime::from_file(path) {
        Ok(new_runtime) => {
            set_lua_keybinds(new_runtime.published_keybinds());
            *runtime = new_runtime;
            info!("Reloaded Lua script {}", path.display());
            flash(to_main, "Lua reloaded".to_string(), 1.5);
        }
        Err(err) => {
            error!("Reloading Lua script '{}': {err}", path.display());
            flash(to_main, format!("Lua error: {err}"), 4.0);
        }
    }
}

/// Queues an on-screen message for the main thread to show.
fn flash(to_main: &Sender<FromLua>, message: String, duration: f32) {
    let _ = to_main.send(FromLua::Flash { message, duration });
}

/// The worker thread itself: load, then dispatch whatever arrives until the
/// main thread goes away.
fn run(
    source: LuaSource,
    from_main: &Receiver<ToLua>,
    to_main: &Sender<FromLua>,
    queries: &Sender<QueryRequest>,
    has_handlers: &AtomicBool,
    ready: &Sender<()>,
) {
    let mut runtime = load(&source);
    has_handlers.store(runtime.has_event_handlers(), Ordering::Relaxed);
    let _ = ready.send(());

    // The world, as seen from here: ask, and wait for the main thread to answer.
    // Either send or receive failing means it has gone, which the handler sees
    // as a query error and unwinds from.
    let extract = || -> Result<PaneruQueryState, String> {
        let (reply, answer) = bounded(1);
        queries
            .send(QueryRequest { reply })
            .map_err(|_| SHUTTING_DOWN.to_string())?;
        answer.recv().map_err(|_| SHUTTING_DOWN.to_string())?
    };

    // A receive error means the main thread dropped its sender: time to stop.
    while let Ok(message) = from_main.recv() {
        let effects = match message {
            ToLua::Events(events) => {
                let tables: Vec<_> = events
                    .iter()
                    .filter_map(|event| convert::event_table(runtime.lua(), event))
                    .collect();
                runtime.dispatch_with_query("event dispatch", &extract, || {
                    for (name, table) in &tables {
                        runtime.dispatch_event(name, table);
                    }
                })
            }
            ToLua::Binds { ids, snapshot } => {
                let snapshot = match convert::snapshot_table(runtime.lua(), &snapshot) {
                    Ok(snapshot) => snapshot,
                    Err(err) => {
                        error!("lua state snapshot: {err}");
                        continue;
                    }
                };
                runtime.dispatch_with_query("keybind dispatch", &extract, || {
                    for id in ids {
                        runtime.dispatch_bind(id, snapshot.clone());
                    }
                })
            }
            ToLua::Reload(path) => {
                reload(&mut runtime, &path, to_main);
                has_handlers.store(runtime.has_event_handlers(), Ordering::Relaxed);
                continue;
            }
            ToLua::Shutdown => break,
        };

        has_handlers.store(runtime.has_event_handlers(), Ordering::Relaxed);
        let (commands, flashes) = effects;
        for command in commands {
            let _ = to_main.send(FromLua::Command(command));
        }
        for (message, duration) in flashes {
            flash(to_main, message, duration);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::state::{PaneruActiveState, PaneruVirtualWorkspaceState, PaneruWindowState};

    /// How long a test waits for the worker before calling it wedged. Generous:
    /// it only ever elapses on failure.
    const TIMEOUT: Duration = Duration::from_secs(5);

    fn worker(source: &str) -> LuaWorker {
        LuaWorker::spawn(LuaSource::Inline(source.to_string()))
    }

    /// An empty snapshot, for binds that don't look at one.
    fn snapshot() -> StateSnapshot {
        StateSnapshot {
            focused: None,
            windows: Vec::new(),
        }
    }

    /// A canned state document to answer round-trips with.
    fn test_state() -> PaneruQueryState {
        PaneruQueryState {
            version: 1,
            timestamp: 0,
            active: PaneruActiveState {
                focused_window_id: Some(7),
                focused_app_name: Some("Test App".to_string()),
                ..PaneruActiveState::default()
            },
            virtual_workspaces: vec![PaneruVirtualWorkspaceState {
                number: 1,
                native_workspace_id: 10,
                active: true,
                windows: vec![PaneruWindowState {
                    window_id: 7,
                    bundle_id: "com.example.app".to_string(),
                    app_name: "Test App".to_string(),
                    title: "window".to_string(),
                    focused: true,
                    floating: false,
                    display_id: Some(1),
                    frame: None,
                    visible: true,
                }],
            }],
        }
    }

    /// The next side effect, or a panic naming what we were waiting for.
    fn next_effect(worker: &LuaWorker, what: &str) -> FromLua {
        worker
            .outbox
            .recv_timeout(TIMEOUT)
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    fn next_flash(worker: &LuaWorker, what: &str) -> String {
        match next_effect(worker, what) {
            FromLua::Flash { message, .. } => message,
            FromLua::Command(command) => panic!("expected a flash, got {command:?}"),
        }
    }

    #[test]
    fn bind_dispatch_reaches_the_outbox() {
        let worker = worker(r#"paneru.bind("alt - b", "window balance")"#);
        worker.send_binds(vec![1], snapshot());
        let FromLua::Command(command) = next_effect(&worker, "the bound command") else {
            panic!("expected a command");
        };
        assert!(
            matches!(command, Command::Window(crate::commands::Operation::Balance)),
            "expected a balance command, got {command:?}"
        );
    }

    #[test]
    fn event_dispatch_reaches_the_outbox() {
        let worker = worker(
            r#"paneru.on("space_changed", function(e) paneru.flash(e.type) end)"#,
        );
        assert!(worker.has_event_handlers());
        worker.send_events(vec![LuaEvent::SpaceChanged]);
        assert_eq!(next_flash(&worker, "the event flash"), "space_changed");
    }

    #[test]
    fn query_round_trip_is_served_by_the_host() {
        let worker = worker(
            r#"
            paneru.bind("alt - q", function()
              paneru.flash(paneru.query_active().focused_app_name)
            end)
            "#,
        );
        worker.send_binds(vec![1], snapshot());

        let request = worker
            .queries
            .recv_timeout(TIMEOUT)
            .expect("the handler should have asked for the world");
        request.answer(Ok(test_state()));

        assert_eq!(next_flash(&worker, "the queried app name"), "Test App");
    }

    #[test]
    fn two_queries_in_one_dispatch_cost_one_round_trip() {
        let worker = worker(
            r#"
            paneru.bind("alt - q", function()
              paneru.query_active()
              paneru.query_on_screen()
              paneru.flash("done")
            end)
            "#,
        );
        worker.send_binds(vec![1], snapshot());

        worker
            .queries
            .recv_timeout(TIMEOUT)
            .expect("the first query should arrive")
            .answer(Ok(test_state()));
        assert_eq!(next_flash(&worker, "the handler to finish"), "done");
        assert!(
            worker.queries.try_recv().is_err(),
            "the second query should have been served from the cached extraction"
        );
    }

    #[test]
    fn a_dropped_reply_channel_errors_the_handler_not_the_worker() {
        let worker = worker(
            r#"
            paneru.bind("alt - q", function() paneru.query_active() end)
            paneru.bind("alt - b", "window balance")
            "#,
        );
        worker.send_binds(vec![1], snapshot());
        // Drop the request without answering, as a shutdown would.
        drop(
            worker
                .queries
                .recv_timeout(TIMEOUT)
                .expect("the handler should have asked for the world"),
        );

        // The handler's error is not the worker's: it is still dispatching.
        worker.send_binds(vec![2], snapshot());
        let FromLua::Command(command) = next_effect(&worker, "the next bind") else {
            panic!("expected a command");
        };
        assert!(matches!(
            command,
            Command::Window(crate::commands::Operation::Balance)
        ));
    }

    #[test]
    fn reload_failure_keeps_the_old_runtime() {
        let directory = std::env::temp_dir().join("paneru-lua-worker-reload-failure");
        std::fs::create_dir_all(&directory).unwrap();
        let script = directory.join("init.lua");
        std::fs::write(&script, r#"paneru.bind("alt - b", "window balance")"#).unwrap();

        let worker = LuaWorker::spawn(LuaSource::Path(script.clone()));
        std::fs::write(&script, "this is not lua ===").unwrap();
        worker.send_reload(script.clone());
        assert!(
            next_flash(&worker, "the reload error").starts_with("Lua error:"),
            "a broken script should be reported"
        );

        // ...and the bind registered by the working script still dispatches.
        worker.send_binds(vec![1], snapshot());
        assert!(matches!(
            next_effect(&worker, "the surviving bind"),
            FromLua::Command(Command::Window(crate::commands::Operation::Balance))
        ));

        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn reload_republishes_handlers() {
        let directory = std::env::temp_dir().join("paneru-lua-worker-reload-success");
        std::fs::create_dir_all(&directory).unwrap();
        let script = directory.join("init.lua");
        std::fs::write(&script, r#"paneru.bind("alt - b", "window balance")"#).unwrap();

        let worker = LuaWorker::spawn(LuaSource::Path(script.clone()));
        assert!(!worker.has_event_handlers(), "no paneru.on handlers yet");

        std::fs::write(
            &script,
            r#"paneru.on("space_changed", function(e) paneru.flash("reloaded") end)"#,
        )
        .unwrap();
        worker.send_reload(script.clone());
        assert_eq!(next_flash(&worker, "the reload notice"), "Lua reloaded");
        assert!(
            worker.has_event_handlers(),
            "the reloaded script's handler should be visible to the fast path"
        );

        worker.send_events(vec![LuaEvent::SpaceChanged]);
        assert_eq!(next_flash(&worker, "the new handler"), "reloaded");

        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn dropping_the_handle_stops_the_thread() {
        let mut worker = worker("");
        let thread = worker.thread.take().expect("just spawned");
        drop(worker);
        let mut waited = Duration::ZERO;
        while !thread.is_finished() && waited < TIMEOUT {
            std::thread::sleep(SHUTDOWN_POLL);
            waited += SHUTDOWN_POLL;
        }
        assert!(thread.is_finished(), "the worker should stop with its handle");
    }
}

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
//! ([`LuaEvent`], [`WindowSet`], [`Command`], [`PaneruQueryState`]), which is
//! what the marshalling split in [`super::convert`] exists to make possible.
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

use super::convert::{self, LuaEvent};
use super::runtime::LuaRuntime;
use crate::commands::Command;
use crate::ecs::state::PaneruQueryState;
use crate::platform::input::set_lua_keybinds;
use paneru_shared_types::windowset::WindowSet;

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
    /// One frame's worth of keybind ids.
    Binds {
        ids: Vec<u32>,
    },
    Reload(PathBuf),
    Shutdown,
}

/// A side effect a callback produced, on its way back to the command bus.
pub(super) enum FromLua {
    Command(Command),
    Flash { message: String, duration: f32 },
}

/// A read of the live world that a handler is blocked on. Carries the reply
/// channel and nothing else: what the answer is wanted *for* — which query
/// kind, JSON or table — stays on the worker.
pub(super) enum QueryRequest {
    /// The `paneru.query*` documents.
    State {
        reply: Sender<Result<PaneruQueryState, String>>,
    },
    /// The layout tree a handler transforms.
    WindowSet {
        reply: Sender<Result<WindowSet, String>>,
    },
}

#[cfg(test)]
impl QueryRequest {
    /// Answers a state query. Panics on a window-set request, which the tests
    /// using this never make.
    fn answer(self, state: Result<PaneruQueryState, String>) {
        match self {
            QueryRequest::State { reply } => {
                let _ = reply.send(state);
            }
            QueryRequest::WindowSet { .. } => panic!("expected a state query"),
        }
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
                .spawn(move || {
                    run(
                        &source,
                        &from_main,
                        &to_main,
                        &query_tx,
                        &has_handlers,
                        &ready_tx,
                    );
                })
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
    pub(super) fn send_binds(&self, ids: Vec<u32>) {
        let _ = self.to_lua.send(ToLua::Binds { ids });
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
    source: &LuaSource,
    from_main: &Receiver<ToLua>,
    to_main: &Sender<FromLua>,
    queries: &Sender<QueryRequest>,
    has_handlers: &AtomicBool,
    ready: &Sender<()>,
) {
    let mut runtime = load(source);
    has_handlers.store(runtime.has_event_handlers(), Ordering::Relaxed);
    let _ = ready.send(());

    // The world, as seen from here: ask, and wait for the main thread to answer.
    // Either send or receive failing means it has gone, which the handler sees
    // as a query error and unwinds from.
    let extract = || -> Result<PaneruQueryState, String> {
        let (reply, answer) = bounded(1);
        queries
            .send(QueryRequest::State { reply })
            .map_err(|_| SHUTTING_DOWN.to_string())?;
        answer.recv().map_err(|_| SHUTTING_DOWN.to_string())?
    };
    let extract_set = || -> Result<WindowSet, String> {
        let (reply, answer) = bounded(1);
        queries
            .send(QueryRequest::WindowSet { reply })
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
                runtime.dispatch_with_query("event dispatch", &extract, &extract_set, || {
                    for (name, table) in &tables {
                        runtime.dispatch_event(name, table);
                    }
                })
            }
            ToLua::Binds { ids } => {
                runtime.dispatch_with_query("keybind dispatch", &extract, &extract_set, || {
                    for id in ids {
                        runtime.dispatch_bind(id);
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
    use paneru_shared_types::windowset::{LayoutOp, WinID};

    /// How long a test waits for the worker before calling it wedged. Generous:
    /// it only ever elapses on failure.
    const TIMEOUT: Duration = Duration::from_secs(5);

    fn worker(source: &str) -> LuaWorker {
        LuaWorker::spawn(LuaSource::Inline(source.to_string()))
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
        worker.send_binds(vec![1]);
        let FromLua::Command(command) = next_effect(&worker, "the bound command") else {
            panic!("expected a command");
        };
        assert!(
            matches!(
                command,
                Command::Window(crate::commands::Operation::Balance)
            ),
            "expected a balance command, got {command:?}"
        );
    }

    #[test]
    fn event_dispatch_reaches_the_outbox() {
        let worker = worker(r#"paneru.on("space_changed", function(e) paneru.flash(e.type) end)"#);
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
        worker.send_binds(vec![1]);

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
        worker.send_binds(vec![1]);

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
        worker.send_binds(vec![1]);
        // Drop the request without answering, as a shutdown would.
        drop(
            worker
                .queries
                .recv_timeout(TIMEOUT)
                .expect("the handler should have asked for the world"),
        );

        // The handler's error is not the worker's: it is still dispatching.
        worker.send_binds(vec![2]);
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
        worker.send_binds(vec![1]);
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

    /// The canned layout with its window on workspace 1.
    fn test_window_set() -> WindowSet {
        test_window_set_on(1)
    }

    /// A layout built from `(id, app, workspace)` triples, over workspaces 1
    /// (on screen) and 9 (the stash). Each window gets a column of its own.
    fn layout(windows: &[(WinID, &str, u32)]) -> WindowSet {
        use paneru_shared_types::state::Frame;

        layout_on(
            1,
            Frame {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            windows,
        )
    }

    /// The same layout, on a display of a given id and geometry — what a
    /// proportional placement has to be resolved against.
    fn layout_on(
        display_id: u32,
        display_frame: paneru_shared_types::state::Frame,
        windows: &[(WinID, &str, u32)],
    ) -> WindowSet {
        use paneru_shared_types::windowset::{ColumnSet, DisplaySet, WindowRec, WorkspaceSet};

        let workspaces = [1, 9]
            .map(|number| WorkspaceSet {
                number,
                native_id: 10,
                active: number == 1,
                columns: Arc::new(
                    windows
                        .iter()
                        .filter(|(_, _, on)| *on == number)
                        .map(|(id, app, _)| {
                            ColumnSet::single(
                                WindowRec {
                                    id: *id,
                                    app_name: (*app).to_string(),
                                    bundle_id: format!("com.example.{app}"),
                                    title: format!("{app} window"),
                                    frame: None,
                                    floating: false,
                                    managed: true,
                                    visible: number == 1,
                                    focused: false,
                                },
                                1.0,
                            )
                        })
                        .collect(),
                ),
                floating: Arc::new(Vec::new()),
            })
            .to_vec();

        WindowSet::new(
            vec![DisplaySet {
                id: display_id,
                frame: display_frame,
                active: true,
                workspaces: Arc::new(workspaces),
            }],
            None,
        )
    }

    /// Answers the next window-set request with `set`.
    fn serve(worker: &LuaWorker, set: WindowSet) {
        match worker
            .queries
            .recv_timeout(TIMEOUT)
            .expect("the handler should have asked for the window set")
        {
            QueryRequest::WindowSet { reply } => {
                let _ = reply.send(Ok(set));
            }
            QueryRequest::State { .. } => panic!("expected a window-set request"),
        }
    }

    /// The ops of the next layout command.
    fn next_ops(worker: &LuaWorker, what: &str) -> Vec<LayoutOp> {
        match next_effect(worker, what) {
            FromLua::Command(Command::Layout(ops)) => ops,
            other => match other {
                FromLua::Flash { message, .. } => panic!("expected ops, got flash {message:?}"),
                FromLua::Command(command) => panic!("expected ops, got {command:?}"),
            },
        }
    }

    /// The canned layout with its window on `holding`: one display, workspace 1
    /// on screen and workspace 9 as somewhere to stash things.
    ///
    /// Built directly rather than by transforming, because a transform would
    /// record an op — and a set handed to a handler has asked for nothing yet,
    /// which is what the real extractor produces.
    fn test_window_set_on(holding: u32) -> WindowSet {
        use paneru_shared_types::state::Frame;
        use paneru_shared_types::windowset::{ColumnSet, DisplaySet, WindowRec, WorkspaceSet};

        let window = WindowRec {
            id: 7,
            app_name: "Test App".to_string(),
            bundle_id: "com.example.app".to_string(),
            title: "window".to_string(),
            frame: None,
            floating: false,
            managed: true,
            visible: true,
            focused: true,
        };
        WindowSet::new(
            vec![DisplaySet {
                id: 1,
                frame: Frame {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
                active: true,
                workspaces: Arc::new(
                    [1, 9]
                        .map(|number| WorkspaceSet {
                            number,
                            native_id: 10,
                            active: number == 1,
                            columns: Arc::new(if number == holding {
                                vec![ColumnSet::single(window.clone(), 1.0)]
                            } else {
                                Vec::new()
                            }),
                            floating: Arc::new(Vec::new()),
                        })
                        .to_vec(),
                ),
            }],
            Some(7),
        )
    }

    /// Answers the next window-set request, or panics saying what arrived.
    fn serve_window_set(worker: &LuaWorker) {
        match worker
            .queries
            .recv_timeout(TIMEOUT)
            .expect("the handler should have asked for the window set")
        {
            QueryRequest::WindowSet { reply } => {
                let _ = reply.send(Ok(test_window_set()));
            }
            QueryRequest::State { .. } => panic!("expected a window-set request"),
        }
    }

    #[test]
    fn a_returned_window_set_commits_its_operations() {
        let worker =
            worker(r#"paneru.bind("alt - f", function(ws) return ws:focus(ws:focused()) end)"#);
        worker.send_binds(vec![1]);
        serve_window_set(&worker);

        let FromLua::Command(command) = next_effect(&worker, "the layout command") else {
            panic!("expected a command");
        };
        let Command::Layout(ops) = command else {
            panic!("expected a layout command, got {command:?}");
        };
        assert_eq!(ops, vec![LayoutOp::Focus(7)]);
    }

    #[test]
    fn a_window_set_computed_but_not_returned_commits_nothing() {
        // The whole point of the value being pure: work you throw away has no
        // consequences. The handler transforms, discards, and flashes instead.
        let worker = worker(
            r#"
            paneru.bind("alt - f", function(ws)
              local unused = ws:focus(ws:focused()):view(2)
              paneru.flash("discarded")
            end)
            "#,
        );
        worker.send_binds(vec![1]);
        serve_window_set(&worker);

        assert_eq!(next_flash(&worker, "the handler to finish"), "discarded");
        assert!(
            worker.outbox.try_recv().is_err(),
            "an unreturned window set should commit nothing"
        );
    }

    #[test]
    fn a_handler_that_raises_after_transforming_commits_nothing() {
        let worker = worker(
            r#"
            paneru.bind("alt - f", function(ws)
              local pending = ws:focus(ws:focused())
              error("nope")
            end)
            paneru.bind("alt - b", "window balance")
            "#,
        );
        worker.send_binds(vec![1]);
        serve_window_set(&worker);

        // Nothing from the failed handler; the worker is still dispatching.
        worker.send_binds(vec![2]);
        assert!(matches!(
            next_effect(&worker, "the next bind"),
            FromLua::Command(Command::Window(crate::commands::Operation::Balance))
        ));
    }

    #[test]
    fn chained_transforms_commit_in_order() {
        let worker = worker(
            r#"
            paneru.bind("alt - x", function(ws)
              return ws:focus(7):width(7, 0.75):shift(7, 2)
            end)
            "#,
        );
        worker.send_binds(vec![1]);
        serve_window_set(&worker);

        let FromLua::Command(Command::Layout(ops)) = next_effect(&worker, "the layout command")
        else {
            panic!("expected a layout command");
        };
        assert_eq!(
            ops,
            vec![
                LayoutOp::Focus(7),
                LayoutOp::SetWidth {
                    window: 7,
                    ratio: 0.75
                },
                LayoutOp::MoveToWorkspace {
                    window: 7,
                    workspace: 2,
                    follow: false
                },
            ]
        );
    }

    #[test]
    fn a_handler_that_ignores_the_window_set_never_fetches_one() {
        // Laziness is what keeps the window set affordable on hot events: it
        // costs a round-trip and reads every window title over the AX API.
        let worker = worker(r#"paneru.bind("alt - b", "window balance")"#);
        worker.send_binds(vec![1]);

        assert!(matches!(
            next_effect(&worker, "the bound command"),
            FromLua::Command(Command::Window(crate::commands::Operation::Balance))
        ));
        assert!(
            worker.queries.try_recv().is_err(),
            "a handler that never touches the window set should not ask for one"
        );
    }

    #[test]
    fn two_handlers_in_one_batch_share_one_window_set() {
        let worker = worker(
            r#"
            paneru.bind("alt - a", function(ws) paneru.flash(tostring(ws:focused())) end)
            paneru.bind("alt - b", function(ws) paneru.flash(tostring(ws:focused())) end)
            "#,
        );
        worker.send_binds(vec![1, 2]);
        serve_window_set(&worker);

        assert_eq!(next_flash(&worker, "the first handler"), "7");
        assert_eq!(next_flash(&worker, "the second handler"), "7");
        assert!(
            worker.queries.try_recv().is_err(),
            "the second handler should have reused the first fetch"
        );
    }

    #[test]
    fn event_handlers_receive_the_event_then_the_window_set() {
        let worker = worker(
            r#"
            paneru.on("space_changed", function(event, ws)
              paneru.flash(event.type .. ":" .. tostring(ws:focused()))
            end)
            "#,
        );
        worker.send_events(vec![LuaEvent::SpaceChanged]);
        serve_window_set(&worker);
        assert_eq!(next_flash(&worker, "the event handler"), "space_changed:7");
    }

    #[test]
    fn a_captured_window_set_stays_the_snapshot_it_was() {
        // It is a value, not a view: a handler that keeps one sees what it saw,
        // and does not silently re-read the world later.
        let worker = worker(
            r#"
            escaped = nil
            paneru.bind("alt - a", function(ws)
              escaped = ws
              paneru.flash(tostring(ws:focused()))
            end)
            paneru.bind("alt - b", function() paneru.flash(tostring(escaped:focused())) end)
            "#,
        );
        worker.send_binds(vec![1]);
        serve_window_set(&worker);
        assert_eq!(next_flash(&worker, "the first handler"), "7");

        worker.send_binds(vec![2]);
        assert_eq!(next_flash(&worker, "the captured set"), "7");
        assert!(
            worker.queries.try_recv().is_err(),
            "reading a captured set should not go back to the world"
        );
    }

    /// The scratchpad module documented in CONFIGURATION.md, kept here so the
    /// documented code is the code under test. Modelled on xmonad's
    /// `XMonad.Util.NamedScratchpad`.
    const SCRATCHPAD: &str = r#"
        scratchpad = { stash = 9, pads = {}, order = {} }

        function scratchpad.define(name, spec)
          scratchpad.pads[name] = spec
          table.insert(scratchpad.order, name)
        end

        -- The pad a window belongs to, if any. Declaration order decides ties.
        function scratchpad.pad_of(window)
          for _, name in ipairs(scratchpad.order) do
            if scratchpad.pads[name].match(window) then
              return name, scratchpad.pads[name]
            end
          end
        end

        -- Park every pad in `names` that is currently on screen.
        function scratchpad.hide(ws, names)
          for _, name in ipairs(names) do
            local window = ws:find(scratchpad.pads[name].match)
            if window and ws:workspace_of(window.id) == ws:current() then
              ws = ws:shift(window.id, scratchpad.stash)
            end
          end
          return ws
        end

        function scratchpad.hide_all(ws)
          return scratchpad.hide(ws, scratchpad.order)
        end

        -- Everything declared in the same group as `name`, except itself.
        function scratchpad.group_of(name)
          local group, mine = {}, scratchpad.pads[name].group
          if not mine then return group end
          for _, other in ipairs(scratchpad.order) do
            if other ~= name and scratchpad.pads[other].group == mine then
              table.insert(group, other)
            end
          end
          return group
        end

        function scratchpad.toggle(name)
          return function(ws)
            local pad = scratchpad.pads[name]
            local window = ws:find(pad.match)
            if not window then
              os.execute(pad.spawn .. " &")
              return
            end
            if ws:workspace_of(window.id) == ws:current() then
              return ws:shift(window.id, scratchpad.stash)
            end
            ws = scratchpad.hide(ws, scratchpad.group_of(name))
            return ws:shift(window.id, ws:current(), true):focus(window.id)
          end
        end

        -- The manage hook: place a pad window the first time we see it.
        scratchpad.seen = {}
        paneru.on("window_focused", function(event, ws)
          if scratchpad.seen[event.window_id] then return end
          scratchpad.seen[event.window_id] = true
          local window = ws:window(event.window_id)
          if not window then return end
          local _, pad = scratchpad.pad_of(window)
          if pad and pad.float and window.managed then
            return ws:float(window.id, pad.float)
          end
        end)

        -- Hide a pad when the focus leaves it.
        scratchpad.focused = nil
        paneru.on("window_focused", function(event, ws)
          local previous = scratchpad.focused
          scratchpad.focused = event.window_id
          if not previous or previous == event.window_id then return end
          local window = ws:window(previous)
          if window and scratchpad.pad_of(window) then
            return ws:shift(previous, scratchpad.stash)
          end
        end)

        scratchpad.define("terminal", {
          match = paneru.match{ app = "Alacritty" },
          spawn = "true", group = "console",
          float = { x = 0.1, y = 0.05, width = 0.8, height = 0.5 },
        })
        scratchpad.define("notes", {
          match = paneru.match{ app = "Obsidian" },
          spawn = "true", group = "console",
        })

        paneru.bind("alt - s", scratchpad.toggle("terminal"))
        paneru.bind("alt - n", scratchpad.toggle("notes"))
        paneru.bind("alt - 0", scratchpad.hide_all)

        -- A sentinel for the tests: touches nothing, so anything queued ahead
        -- of its flash is something a handler actually asked for.
        paneru.bind("alt - z", function() paneru.flash("sentinel") end)
    "#;

    /// Asserts nothing was queued: dispatches a handler that only flashes, and
    /// requires that flash to be the very next thing out of the outbox.
    ///
    /// A bare `try_recv` would race the worker, which may not have finished the
    /// dispatch under test yet.
    fn assert_nothing_queued(worker: &LuaWorker) {
        worker.send_binds(vec![4]);
        assert_eq!(
            next_flash(worker, "the sentinel"),
            "sentinel",
            "a handler queued something it should not have"
        );
    }

    #[test]
    fn a_scratchpad_on_screen_is_parked() {
        let worker = worker(SCRATCHPAD);
        worker.send_binds(vec![1]);
        serve(&worker, layout(&[(7, "Alacritty", 1)]));

        assert_eq!(
            next_ops(&worker, "the stash"),
            vec![LayoutOp::MoveToWorkspace {
                window: 7,
                workspace: 9,
                follow: false
            }]
        );
    }

    #[test]
    fn a_stashed_scratchpad_is_summoned_and_focused() {
        let worker = worker(SCRATCHPAD);
        worker.send_binds(vec![1]);
        serve(&worker, layout(&[(7, "Alacritty", 9)]));

        assert_eq!(
            next_ops(&worker, "the summons"),
            vec![
                LayoutOp::MoveToWorkspace {
                    window: 7,
                    workspace: 1,
                    follow: true
                },
                LayoutOp::Focus(7),
            ]
        );
    }

    #[test]
    fn a_scratchpad_that_is_not_running_is_spawned_and_nothing_moves() {
        let worker = worker(SCRATCHPAD);
        worker.send_binds(vec![1]);
        serve(&worker, layout(&[(3, "Something Else", 1)]));

        // `spawn` is `true`, so the only observable effect is that no layout
        // command is issued.
        assert_nothing_queued(&worker);
    }

    #[test]
    fn summoning_a_scratchpad_hides_its_exclusive_group() {
        let worker = worker(SCRATCHPAD);
        // Notes is on screen; summoning the terminal from the stash should put
        // notes away first.
        worker.send_binds(vec![1]);
        serve(&worker, layout(&[(7, "Alacritty", 9), (8, "Obsidian", 1)]));

        assert_eq!(
            next_ops(&worker, "the exclusive swap"),
            vec![
                LayoutOp::MoveToWorkspace {
                    window: 8,
                    workspace: 9,
                    follow: false
                },
                LayoutOp::MoveToWorkspace {
                    window: 7,
                    workspace: 1,
                    follow: true
                },
                LayoutOp::Focus(7),
            ]
        );
    }

    #[test]
    fn hide_all_parks_every_visible_scratchpad() {
        let worker = worker(SCRATCHPAD);
        worker.send_binds(vec![3]);
        serve(
            &worker,
            layout(&[(7, "Alacritty", 1), (8, "Obsidian", 1), (9, "Mail", 1)]),
        );

        // Both pads, and nothing else.
        assert_eq!(
            next_ops(&worker, "the sweep"),
            vec![
                LayoutOp::MoveToWorkspace {
                    window: 7,
                    workspace: 9,
                    follow: false
                },
                LayoutOp::MoveToWorkspace {
                    window: 8,
                    workspace: 9,
                    follow: false
                },
            ]
        );
    }

    #[test]
    fn the_manage_hook_floats_a_pad_window_once() {
        let worker = worker(SCRATCHPAD);
        worker.send_events(vec![LuaEvent::WindowFocused { window_id: 7 }]);
        serve(&worker, layout(&[(7, "Alacritty", 1)]));

        assert_eq!(
            next_ops(&worker, "the float"),
            vec![
                LayoutOp::SetFloating {
                    window: 7,
                    floating: true
                },
                // 0.1/0.05/0.8/0.5 of the fixture's 1920x1080 display.
                LayoutOp::SetFrame {
                    window: 7,
                    frame: paneru_shared_types::state::Frame {
                        x: 192,
                        y: 54,
                        width: 1536,
                        height: 540
                    }
                },
            ]
        );

        // Focusing it again is neither a new window nor a focus change, so
        // both handlers bail before touching the set — which is why this needs
        // no second `serve`: nothing asks for one.
        worker.send_events(vec![LuaEvent::WindowFocused { window_id: 7 }]);
        assert_nothing_queued(&worker);
    }

    #[test]
    fn a_pad_with_no_rect_is_not_placed_at_all() {
        // `notes` declares no `float`, so the manage hook leaves it tiled: no
        // float, and above all no frame invented for it.
        let worker = worker(SCRATCHPAD);
        worker.send_events(vec![LuaEvent::WindowFocused { window_id: 8 }]);
        serve(&worker, layout(&[(8, "Obsidian", 1)]));
        assert_nothing_queued(&worker);
    }

    #[test]
    fn a_pad_is_placed_on_the_display_it_is_on() {
        // The same fractions, resolved against a second display: proportional
        // placement is what makes one pad definition work on both.
        let worker = worker(SCRATCHPAD);
        worker.send_events(vec![LuaEvent::WindowFocused { window_id: 7 }]);
        serve(
            &worker,
            layout_on(
                2,
                paneru_shared_types::state::Frame {
                    x: 1920,
                    y: -200,
                    width: 1280,
                    height: 800,
                },
                &[(7, "Alacritty", 1)],
            ),
        );

        assert_eq!(
            next_ops(&worker, "the float"),
            vec![
                LayoutOp::SetFloating {
                    window: 7,
                    floating: true
                },
                // 0.1/0.05/0.8/0.5 of 1280x800, offset by the display origin.
                LayoutOp::SetFrame {
                    window: 7,
                    frame: paneru_shared_types::state::Frame {
                        x: 2048,
                        y: -160,
                        width: 1024,
                        height: 400
                    }
                },
            ]
        );
    }

    #[test]
    fn a_placed_pad_still_stashes_and_summons() {
        // The placement is a manage-hook concern; toggling it in and out of
        // view stays a workspace move, and does not re-place the window.
        let worker = worker(SCRATCHPAD);
        worker.send_events(vec![LuaEvent::WindowFocused { window_id: 7 }]);
        serve(&worker, layout(&[(7, "Alacritty", 1)]));
        assert_eq!(next_ops(&worker, "the float").len(), 2, "float then place");

        worker.send_binds(vec![1]);
        serve(&worker, layout(&[(7, "Alacritty", 1)]));
        assert_eq!(
            next_ops(&worker, "the stash"),
            vec![LayoutOp::MoveToWorkspace {
                window: 7,
                workspace: 9,
                follow: false
            }]
        );

        worker.send_binds(vec![1]);
        serve(&worker, layout(&[(7, "Alacritty", 9)]));
        assert_eq!(
            next_ops(&worker, "the summons"),
            vec![
                LayoutOp::MoveToWorkspace {
                    window: 7,
                    workspace: 1,
                    follow: true
                },
                LayoutOp::Focus(7),
            ]
        );
    }

    #[test]
    fn losing_focus_parks_a_scratchpad() {
        let worker = worker(SCRATCHPAD);
        // Focus the pad, then focus something else.
        worker.send_events(vec![LuaEvent::WindowFocused { window_id: 7 }]);
        serve(&worker, layout(&[(7, "Alacritty", 1), (8, "Mail", 1)]));
        assert_eq!(
            next_ops(&worker, "the float").first(),
            Some(&LayoutOp::SetFloating {
                window: 7,
                floating: true
            })
        );

        worker.send_events(vec![LuaEvent::WindowFocused { window_id: 8 }]);
        serve(&worker, layout(&[(7, "Alacritty", 1), (8, "Mail", 1)]));
        // Two handlers are registered for this event and each commits its own
        // result: the manage hook passes on a non-pad window, the focus-loss
        // hook parks the one that lost it.
        assert_eq!(
            next_ops(&worker, "the park"),
            vec![LayoutOp::MoveToWorkspace {
                window: 7,
                workspace: 9,
                follow: false
            }]
        );
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
        assert!(
            thread.is_finished(),
            "the worker should stop with its handle"
        );
    }
}

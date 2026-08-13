//! Runs the Lua interpreter on a thread of its own.
//!
//! `mlua::Lua` is `!Send`, and a handler is arbitrary script code of unbounded
//! duration, so it cannot run on the main thread without blocking window
//! management for as long as it takes. The main thread only sends
//! ([`ToLua`]) and drains ([`FromLua`]) over unbounded, non-blocking
//! channels; world reads ([`WorldRequest`]) and script-state access
//! ([`StoreRequest`]) go through their own reply channels, so a handler
//! awaiting an answer never blocks the others. Nothing crossing either
//! channel is a Lua value — only plain data ([`LuaEvent`], [`WindowSet`],
//! [`Command`], [`PaneruQueryState`]); see [`super::convert`] for the
//! marshalling.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use std::cell::RefCell;
use std::rc::Rc;

use async_channel::{Receiver, Sender, bounded, unbounded};
use async_executor::LocalExecutor;
use bevy::ecs::resource::Resource;
use futures_lite::future::block_on;
use tracing::{error, info, warn};

use super::convert::{self, LuaEvent};
use super::runtime::LuaRuntime;
use super::world::{DispatchWorld, WorldAccess};
use crate::commands::Command;
use crate::config::Config;
use crate::ecs::state::PaneruQueryState;
use crate::platform::input::set_lua_keybinds;
use paneru_shared_types::script_state::{ScriptState, ScriptStateWrite, WriteOutcome};
use paneru_shared_types::windowset::WindowSet;

/// How long [`Drop`] waits for an in-flight dispatch to finish before giving
/// up and detaching the thread. Bounded, so a script stuck in a loop can
/// never stop the process from exiting.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);
const SHUTDOWN_POLL: Duration = Duration::from_millis(2);

/// Where a runtime's script comes from — the worker builds the interpreter
/// itself, so it needs the source rather than the built runtime.
pub enum LuaSource {
    Path(PathBuf),
    /// Source given directly; only used by tests.
    #[cfg(test)]
    Inline(String),
}

/// Work for the interpreter. Unbounded and FIFO, so a reload can never
/// overtake events queued before it.
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
    Flash {
        message: String,
        duration: f32,
    },
    /// A reload installed a script that calls `paneru.setup{...}`; tells the
    /// main thread to re-apply the config in [`LuaWorker::built_config`].
    ConfigChanged,
}

/// One world read, as every waiter sees it: an `Arc` so one extraction can
/// answer several waiting handlers, and a `Result` so a failed read shares
/// the same way as a successful one.
pub(super) type Shared<T> = Result<Arc<T>, String>;

/// A read of the live ECS world that a handler is waiting on, carrying only
/// the reply channel.
///
/// Kept on a separate channel from [`StoreRequest`] so each can be served by
/// a different system: Bevy grants a system static access to everything its
/// parameters mention, so one system serving both would hold read access to
/// the whole world and exclusive access to the state store on every pass.
pub(super) enum WorldRequest {
    /// The `paneru.query*` documents.
    State {
        reply: Sender<Shared<PaneruQueryState>>,
    },
    /// The layout tree a handler transforms.
    WindowSet { reply: Sender<Shared<WindowSet>> },
}

/// A read or write of the script state store. See [`WorldRequest`] for why this
/// is a separate channel rather than two more variants.
pub(super) enum StoreRequest {
    /// The store, whenever the worker's cached copy is stale.
    Read {
        reply: Sender<Result<ScriptState, String>>,
    },
    /// One write against the store.
    ///
    /// Unlike a command, a write waits for its answer: the store has a
    /// second writer (a socket client), and `paneru.state.mutate` needs to
    /// know whether it was overtaken while the handler can still retry.
    Write {
        write: ScriptStateWrite,
        reply: Sender<Result<WriteOutcome, String>>,
    },
}

#[cfg(test)]
impl WorldRequest {
    /// Answers a state query. Panics on a window-set request, which the tests
    /// using this never make.
    fn answer(self, state: Shared<PaneruQueryState>) {
        match self {
            WorldRequest::State { reply } => {
                let _ = reply.try_send(state);
            }
            WorldRequest::WindowSet { .. } => panic!("expected a state query"),
        }
    }
}

/// The main thread's handle on the interpreter.
///
/// Unlike the runtime it stands for, this is `Send + Sync`, so every system
/// can take it as `Res<LuaWorker>` rather than `ResMut`.
#[derive(Resource)]
pub struct LuaWorker {
    to_lua: Sender<ToLua>,
    outbox: Receiver<FromLua>,
    /// Reads of the ECS world, and store traffic, on separate channels so
    /// separate systems can serve them. See [`WorldRequest`].
    world_queries: Receiver<WorldRequest>,
    store_queries: Receiver<StoreRequest>,
    /// Mirrors the runtime's `has_event_handlers`, republished after every
    /// load and reload, so the main thread's fast path never has to ask and
    /// wait.
    has_handlers: Arc<AtomicBool>,
    /// The `Config` the loaded script declared through `paneru.setup{...}`,
    /// or `None` if it left configuration to the TOML file.
    built_config: Arc<Mutex<Option<Config>>>,
    thread: Option<JoinHandle<()>>,
}

/// How the worker learns that the script state store has moved under it: the
/// worker caches the store and only re-reads it when this stamp no longer
/// matches the one its copy was taken at.
pub type ScriptStateRevision = Arc<AtomicU64>;

impl LuaWorker {
    /// Starts the worker and waits for it to finish loading `source`, so a
    /// script error is reported at startup, keybinds are published before
    /// the event tap can see a keypress, and a broken script still leaves a
    /// working (empty) runtime behind.
    ///
    /// `revision` is the script state store's stamp, shared with the ECS
    /// resource that owns the store.
    pub fn spawn(source: LuaSource, revision: ScriptStateRevision) -> Self {
        let (to_lua, from_main) = unbounded();
        let (to_main, outbox) = unbounded();
        let (world_tx, world_queries) = unbounded();
        let (store_tx, store_queries) = unbounded();
        let (ready_tx, ready) = bounded(1);
        let has_handlers = Arc::new(AtomicBool::new(false));
        let built_config = Arc::new(Mutex::new(None));

        let thread = {
            let has_handlers = Arc::clone(&has_handlers);
            let built_config = Arc::clone(&built_config);
            std::thread::Builder::new()
                .name("paneru-lua".to_string())
                .spawn(move || {
                    run(
                        &source,
                        &from_main,
                        &to_main,
                        &world_tx,
                        &store_tx,
                        &has_handlers,
                        &built_config,
                        &revision,
                        &ready_tx,
                    );
                })
                .expect("spawning the Lua worker thread")
        };
        // An error here means the thread died before finishing the load, which
        // `run` only does after logging why.
        let _ = ready.recv_blocking();

        Self {
            to_lua,
            outbox,
            world_queries,
            store_queries,
            has_handlers,
            built_config,
            thread: Some(thread),
        }
    }

    /// The `Config` the loaded script declared via `paneru.setup{...}`, or
    /// `None` if it never called it. Safe to read as soon as [`spawn`]
    /// returns, which waits for the load to finish.
    ///
    /// [`spawn`]: LuaWorker::spawn
    pub fn built_config(&self) -> Option<Config> {
        self.built_config
            .lock()
            .expect("the Lua worker never panics while holding this")
            .clone()
    }

    /// Whether the loaded script registered any `paneru.on` handler.
    pub(super) fn has_event_handlers(&self) -> bool {
        self.has_handlers.load(Ordering::Relaxed)
    }

    /// Queues events for dispatch. Never blocks; a send only fails once the
    /// worker is gone.
    pub(super) fn send_events(&self, events: Vec<LuaEvent>) {
        let _ = self.to_lua.try_send(ToLua::Events(events));
    }

    /// Queues keybind callbacks for dispatch.
    pub(super) fn send_binds(&self, ids: Vec<u32>) {
        let _ = self.to_lua.try_send(ToLua::Binds { ids });
    }

    /// Asks the worker to rebuild itself from `path`.
    pub(super) fn send_reload(&self, path: PathBuf) {
        let _ = self.to_lua.try_send(ToLua::Reload(path));
    }

    /// The side effects callbacks have produced since the last drain.
    pub(super) fn drain_outbox(&self) -> impl Iterator<Item = FromLua> + '_ {
        std::iter::from_fn(|| self.outbox.try_recv().ok())
    }

    /// The `paneru.query*` and window-set calls currently waiting on the world.
    pub(super) fn pending_world_queries(&self) -> impl Iterator<Item = WorldRequest> + '_ {
        std::iter::from_fn(|| self.world_queries.try_recv().ok())
    }

    /// The `paneru.state` calls currently waiting on the store.
    pub(super) fn pending_store_queries(&self) -> impl Iterator<Item = StoreRequest> + '_ {
        std::iter::from_fn(|| self.store_queries.try_recv().ok())
    }
}

impl Drop for LuaWorker {
    fn drop(&mut self) {
        let _ = self.to_lua.try_send(ToLua::Shutdown);
        let Some(thread) = self.thread.take() else {
            return;
        };
        // Dropping the query receiver unblocks a handler waiting on a reply:
        // its sender dies with the queue, so `recv` errors instead of
        // waiting forever.
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

/// Builds the runtime for `source`, falling back to an empty one (and
/// logging) if the script errors, so a later reload can still install a fix.
/// Always publishes the resulting keybinds via [`set_lua_keybinds`].
fn load(source: &LuaSource, world: &Rc<DispatchWorld>) -> LuaRuntime {
    let runtime = match source {
        LuaSource::Path(path) => match LuaRuntime::from_file(path, world) {
            Ok(runtime) => {
                info!("Loaded Lua script {}", path.display());
                runtime
            }
            Err(err) => {
                warn!("Loading Lua script '{}': {err}", path.display());
                LuaRuntime::empty(world)
            }
        },
        #[cfg(test)]
        LuaSource::Inline(source) => LuaRuntime::from_source(source, world).unwrap_or_else(|err| {
            warn!("Loading inline Lua source: {err}");
            LuaRuntime::empty(world)
        }),
    };
    set_lua_keybinds(runtime.published_keybinds());
    runtime
}

/// Rebuilds the runtime from `path`, committing only on success so a broken
/// edit never tears down the working setup.
///
/// The new runtime is handed back rather than written through a `&mut`: a
/// dispatch suspended mid-await still holds an `Rc` to the old one, so a reload
/// swaps what *subsequent* dispatches see and lets those in flight finish
/// against the interpreter they started on.
fn reload(
    path: &Path,
    world: &Rc<DispatchWorld>,
    to_main: &Sender<FromLua>,
    built_config: &Mutex<Option<Config>>,
) -> Option<LuaRuntime> {
    match LuaRuntime::from_file(path, world) {
        Ok(runtime) => {
            set_lua_keybinds(runtime.published_keybinds());
            // A reloaded `paneru.setup{...}` stays authoritative: if the
            // edited script dropped `setup`, keep the config already in
            // force rather than reverting to TOML.
            if let Some(config) = runtime.built_config() {
                publish_config(built_config, config);
                let _ = to_main.try_send(FromLua::ConfigChanged);
            }
            info!("Reloaded Lua script {}", path.display());
            flash(to_main, "Lua reloaded".to_string(), 1.5);
            Some(runtime)
        }
        Err(err) => {
            error!("Reloading Lua script '{}': {err}", path.display());
            flash(to_main, format!("Lua error: {err}"), 4.0);
            None
        }
    }
}

/// Publishes the config a `paneru.setup{...}` built, for the main thread to read.
fn publish_config(slot: &Mutex<Option<Config>>, config: &Config) {
    *slot
        .lock()
        .expect("the main thread never panics while holding this") = Some(config.clone());
}

/// Queues an on-screen message for the main thread to show.
fn flash(to_main: &Sender<FromLua>, message: String, duration: f32) {
    let _ = to_main.try_send(FromLua::Flash { message, duration });
}

/// The worker thread itself: load, then dispatch whatever arrives until the
/// main thread goes away.
// One parameter per channel and per shared cell, all owned by `spawn`;
// bundling them into a struct would only move the same list elsewhere.
#[allow(clippy::too_many_arguments)]
fn run(
    source: &LuaSource,
    from_main: &Receiver<ToLua>,
    to_main: &Sender<FromLua>,
    world_queries: &Sender<WorldRequest>,
    store_queries: &Sender<StoreRequest>,
    has_handlers: &AtomicBool,
    built_config: &Mutex<Option<Config>>,
    revision: &ScriptStateRevision,
    ready: &Sender<()>,
) {
    let world = DispatchWorld::new(WorldAccess::new(
        world_queries.clone(),
        store_queries.clone(),
        Arc::clone(revision),
    ));
    let loaded = load(source, &world);
    has_handlers.store(loaded.has_event_handlers(), Ordering::Relaxed);
    if let Some(config) = loaded.built_config() {
        publish_config(built_config, config);
    }
    let _ = ready.try_send(());

    // Behind an `Rc` so a reload can replace it without disturbing a dispatch
    // suspended mid-await, and behind a `RefCell` so the replacement is
    // visible afterward. The borrow is never held across an await.
    let current = RefCell::new(Rc::new(loaded));

    // One task per handler, all on this one thread: a handler parked on a
    // world read is not holding the interpreter, so the next one runs
    // instead of queueing behind it.
    let executor = LocalExecutor::new();
    block_on(executor.run(async {
        while let Ok(message) = from_main.recv().await {
            match message {
                ToLua::Events(events) => {
                    let runtime = Rc::clone(&current.borrow());
                    for event in &events {
                        let Some((name, table)) = convert::event_table(runtime.lua(), event) else {
                            continue;
                        };
                        for entry in runtime.event_handlers(&name) {
                            if let Some(ref filter) = entry.filter {
                                match filter.call::<bool>(&table) {
                                    Ok(true) => {}
                                    Ok(false) => continue,
                                    Err(err) => {
                                        error!("lua event filter for '{name}': {err}");
                                        continue;
                                    }
                                }
                            }
                            let task = Task {
                                runtime: Rc::clone(&runtime),
                                to_main: to_main.clone(),
                                has_handlers,
                            };
                            let (name, table, handler) =
                                (name.clone(), table.clone(), entry.handler.clone());
                            executor
                                .spawn(async move {
                                    task.runtime.dispatch_event(&name, &table, &handler).await;
                                    task.finish();
                                })
                                .detach();
                        }
                    }
                }
                ToLua::Binds { ids } => {
                    let runtime = Rc::clone(&current.borrow());
                    for id in ids {
                        let task = Task {
                            runtime: Rc::clone(&runtime),
                            to_main: to_main.clone(),
                            has_handlers,
                        };
                        executor
                            .spawn(async move {
                                task.runtime.dispatch_bind(id).await;
                                task.finish();
                            })
                            .detach();
                    }
                }
                ToLua::Reload(path) => {
                    if let Some(rebuilt) = reload(&path, &world, to_main, built_config) {
                        *current.borrow_mut() = Rc::new(rebuilt);
                    }
                    has_handlers.store(current.borrow().has_event_handlers(), Ordering::Relaxed);
                }
                ToLua::Shutdown => break,
            }
        }
    }));
}

/// One dispatch's share of the worker: the interpreter it runs against, and the
/// way back to the main thread once it is done.
struct Task<'a> {
    runtime: Rc<LuaRuntime>,
    to_main: Sender<FromLua>,
    has_handlers: &'a AtomicBool,
}

impl Task<'_> {
    /// Puts what this dispatch queued on its way to the command bus.
    ///
    /// The outbox is shared, so a dispatch that finishes while another is
    /// suspended may carry that one's commands out with its own — every
    /// command still reaches the bus exactly once, in order, just not
    /// necessarily via the dispatch that queued it.
    fn finish(&self) {
        let (commands, flashes) = self.runtime.drain_outbox();
        for command in commands {
            let _ = self.to_main.try_send(FromLua::Command(command));
        }
        for (message, duration) in flashes {
            flash(&self.to_main, message, duration);
        }
        self.has_handlers
            .store(self.runtime.has_event_handlers(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::state::{PaneruActiveState, PaneruVirtualWorkspaceState, PaneruWindowState};
    use crate::lua::convert::WindowSpawnPayload;
    use paneru_shared_types::windowset::{LayoutOp, WinID};

    /// How long a test waits for the worker before calling it wedged. Generous:
    /// it only ever elapses on failure.
    const TIMEOUT: Duration = Duration::from_secs(5);

    thread_local! {
        /// The store behind the worker under test. Thread-local because each
        /// test runs on its own thread, so a test never sees another's writes.
        static STORE: std::cell::RefCell<TestStore> =
            std::cell::RefCell::new(TestStore::new(revision()));
    }

    fn worker(source: &str) -> LuaWorker {
        spawn_with_store(LuaSource::Inline(source.to_string()))
    }

    /// Spawns a worker with a fresh store behind it, wired to the stamp the
    /// worker watches.
    fn spawn_with_store(source: LuaSource) -> LuaWorker {
        let revision = revision();
        STORE.with_borrow_mut(|store| *store = TestStore::new(Arc::clone(&revision)));
        LuaWorker::spawn(source, revision)
    }

    /// A revision stamp for a test that has no store behind it.
    fn revision() -> ScriptStateRevision {
        Arc::new(AtomicU64::new(0))
    }

    /// Answers everything currently waiting on the store, so a test about
    /// window sets does not have to care that the script also keeps state.
    fn serve_store(worker: &LuaWorker) {
        while let Ok(request) = worker.store_queries.try_recv() {
            STORE.with_borrow_mut(|store| store.answer(request));
        }
    }

    /// The next read of the world, serving the store meanwhile: a handler may
    /// well be waiting on its own state before it gets round to asking for this.
    fn next_world_request(worker: &LuaWorker, what: &str) -> WorldRequest {
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            serve_store(worker);
            if let Ok(request) = worker.world_queries.try_recv() {
                return request;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The main thread's side of the store: what reads are answered from and
    /// what writes land in, with the stamp the worker's cache watches.
    struct TestStore {
        state: ScriptState,
        revision: ScriptStateRevision,
    }

    impl TestStore {
        fn new(revision: ScriptStateRevision) -> Self {
            Self {
                state: ScriptState::default(),
                revision,
            }
        }

        /// Answers one request the way `serve_lua_store` does.
        fn answer(&mut self, request: StoreRequest) {
            match request {
                StoreRequest::Read { reply } => {
                    let _ = reply.try_send(Ok(self.state.clone()));
                }
                StoreRequest::Write { write, reply } => {
                    let outcome = self.state.apply(&write);
                    if matches!(outcome, Ok(WriteOutcome::Applied { changed: true })) {
                        self.revision.fetch_add(1, Ordering::Release);
                    }
                    let _ = reply.try_send(outcome);
                }
            }
        }
    }

    /// Serves the store while waiting for the next effect.
    fn serve_until_effect(worker: &LuaWorker, what: &str) -> FromLua {
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            serve_store(worker);
            if let Ok(request) = worker.world_queries.try_recv() {
                // Every dispatch is handed a window set now, whether or not the
                // handler touches it, so answering that is part of standing in
                // for the main thread rather than something a test opts into.
                match request {
                    WorldRequest::WindowSet { reply } => {
                        let _ = reply.try_send(Ok(Arc::new(test_window_set())));
                    }
                    WorldRequest::State { .. } => {
                        panic!("this test only expects store and window-set requests")
                    }
                }
                continue;
            }
            if let Ok(effect) = worker.outbox.try_recv() {
                return effect;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(2));
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
    ///
    /// Serves the store while it waits, because a handler that writes is
    /// blocked on the answer: waiting only on the outbox would be waiting on a
    /// handler that is waiting on us.
    fn next_effect(worker: &LuaWorker, what: &str) -> FromLua {
        serve_until_effect(worker, what)
    }

    fn next_flash(worker: &LuaWorker, what: &str) -> String {
        match next_effect(worker, what) {
            FromLua::Flash { message, .. } => message,
            FromLua::Command(command) => panic!("expected a flash, got {command:?}"),
            FromLua::ConfigChanged => panic!("expected a flash, got a config change"),
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

        // Every dispatch is handed a window set before the handler runs, so that
        // is the first thing to arrive whether or not the handler wants one.
        serve_window_set(&worker);
        let request = next_world_request(&worker, "the world");
        request.answer(Ok(Arc::new(test_state())));

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

        serve_window_set(&worker);
        next_world_request(&worker, "the first query").answer(Ok(Arc::new(test_state())));
        assert_eq!(next_flash(&worker, "the handler to finish"), "done");
        assert!(
            worker.world_queries.try_recv().is_err(),
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
        drop(next_world_request(&worker, "the world"));

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

        let worker = LuaWorker::spawn(LuaSource::Path(script.clone()), revision());
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

        let worker = LuaWorker::spawn(LuaSource::Path(script.clone()), revision());
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
        match next_world_request(worker, "the window set") {
            WorldRequest::WindowSet { reply } => {
                let _ = reply.try_send(Ok(Arc::new(set)));
            }
            WorldRequest::State { .. } => panic!("expected a window-set request"),
        }
    }

    /// The ops of the next layout command.
    fn next_ops(worker: &LuaWorker, what: &str) -> Vec<LayoutOp> {
        match next_effect(worker, what) {
            FromLua::Command(Command::Layout(ops)) => ops,
            other => match other {
                FromLua::Flash { message, .. } => panic!("expected ops, got flash {message:?}"),
                FromLua::Command(command) => panic!("expected ops, got {command:?}"),
                FromLua::ConfigChanged => panic!("expected ops, got a config change"),
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
        match next_world_request(worker, "the window set") {
            WorldRequest::WindowSet { reply } => {
                let _ = reply.try_send(Ok(Arc::new(test_window_set())));
            }
            WorldRequest::State { .. } => panic!("expected a window-set request"),
        }
    }

    #[test]
    fn script_state_survives_a_reload() {
        let directory = std::env::temp_dir().join("paneru-lua-worker-state-reload");
        std::fs::create_dir_all(&directory).unwrap();
        let script = directory.join("init.lua");
        // Two scripts that share nothing but the store: the first writes, the
        // second reads. A Lua global could not carry a value across this.
        std::fs::write(
            &script,
            r#"paneru.bind("alt - a", function() paneru.state.set("counter", 41) end)"#,
        )
        .unwrap();

        let worker = spawn_with_store(LuaSource::Path(script.clone()));

        worker.send_binds(vec![1]);
        // Serving is what lets the write land; the flash is how we know the
        // handler got that far.
        std::fs::write(
            &script,
            r#"paneru.bind("alt - b", function()
                 paneru.flash("counter=" .. tostring(paneru.state.get("counter")))
               end)"#,
        )
        .unwrap();
        worker.send_reload(script.clone());

        // The write is served along the way; the reload notice is the first
        // effect either script produces.
        let FromLua::Flash { message, .. } = serve_until_effect(&worker, "the reload notice")
        else {
            panic!("expected the reload notice");
        };
        assert_eq!(message, "Lua reloaded");

        worker.send_binds(vec![1]);
        let FromLua::Flash { message, .. } = serve_until_effect(&worker, "the value read back")
        else {
            panic!("expected the flash the reloaded script sends");
        };
        assert_eq!(
            message, "counter=41",
            "a value written before the reload should still be there after it"
        );

        std::fs::remove_dir_all(&directory).ok();
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
            worker.world_queries.try_recv().is_err(),
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
            worker.world_queries.try_recv().is_err(),
            "the second handler should have reused the first fetch"
        );
    }

    /// A handler parked on a world read does not hold up the next one: it is
    /// left waiting on purpose while a second handler runs to completion.
    #[test]
    fn a_handler_waiting_on_the_world_does_not_hold_up_the_next_one() {
        let worker = worker(
            r#"
            paneru.bind("alt - a", function()
              paneru.flash(paneru.query_active().focused_app_name)
            end)
            paneru.bind("alt - b", function() paneru.flash("second") end)
            "#,
        );
        worker.send_binds(vec![1, 2]);
        serve_window_set(&worker);

        // Taken but deliberately not answered: the first handler stays parked.
        let parked = next_world_request(&worker, "the state query");

        assert_eq!(
            next_flash(&worker, "the second handler"),
            "second",
            "the second handler should not be waiting on the first"
        );

        parked.answer(Ok(Arc::new(test_state())));
        assert_eq!(next_flash(&worker, "the first handler"), "Test App");
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
            worker.world_queries.try_recv().is_err(),
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

        -- The manage hook: place a pad window the first time we see it. What
        -- has been seen goes in the store, not a global, so a reload does not
        -- re-run the hook on every open window.
        paneru.on("window_focused", function(event, ws)
          local first_time = false
          paneru.state.mutate("scratchpad.seen", function(seen)
            seen = seen or {}
            first_time = not seen[tostring(event.window_id)]
            seen[tostring(event.window_id)] = true
            return seen
          end)
          if not first_time then return end
          local window = ws:window(event.window_id)
          if not window then return end
          local _, pad = scratchpad.pad_of(window)
          if pad and pad.float and window.managed then
            return ws:float(window.id, pad.float)
          end
        end)

        -- Hide a pad when the focus leaves it.
        paneru.on("window_focused", function(event, ws)
          local previous = paneru.state.get("scratchpad.focused")
          paneru.state.set("scratchpad.focused", event.window_id)
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

    #[test]
    fn window_spawned_event_is_dispatched_to_lua() {
        let worker = worker(
            r#"
            paneru.on("window_spawned", function(event, ws)
                paneru.flash(event.type .. ":" .. tostring(event.window_id) .. ":" .. event.title .. ":" .. event.app_name)
            end)
        "#,
        );
        let event = LuaEvent::WindowSpawned(WindowSpawnPayload {
            window_id: 42,
            pid: 100,
            app_name: "Ghostty".into(),
            bundle_id: "com.mitchellh.ghostty".into(),
            title: "Terminal".into(),
            frame: paneru_shared_types::state::Frame {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            },
            floating: false,
            managed: true,
        });
        worker.send_events(vec![event]);
        assert_eq!(
            next_flash(&worker, "window_spawned"),
            "window_spawned:42:Terminal:Ghostty"
        );
    }

    #[test]
    fn filtered_window_spawned_event_only_fires_on_match() {
        let worker = worker(
            r#"
            paneru.on("window_spawned", { bundle = "libreoffice" }, function(event, ws)
                paneru.flash("matched:" .. event.app_name)
            end)
        "#,
        );
        let ghostty_event = LuaEvent::WindowSpawned(WindowSpawnPayload {
            window_id: 1,
            pid: 100,
            app_name: "Ghostty".into(),
            bundle_id: "com.mitchellh.ghostty".into(),
            title: "Terminal".into(),
            frame: paneru_shared_types::state::Frame {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            },
            floating: false,
            managed: true,
        });
        let libreoffice_event = LuaEvent::WindowSpawned(WindowSpawnPayload {
            window_id: 2,
            pid: 200,
            app_name: "LibreOffice".into(),
            bundle_id: "org.libreoffice.script".into(),
            title: "Document".into(),
            frame: paneru_shared_types::state::Frame {
                x: 0,
                y: 0,
                width: 300,
                height: 300,
            },
            floating: false,
            managed: true,
        });

        // Non-matching event should not trigger flash
        worker.send_events(vec![ghostty_event]);
        assert!(worker.outbox.try_recv().is_err());

        // Matching event should trigger flash
        worker.send_events(vec![libreoffice_event]);
        assert_eq!(
            next_flash(&worker, "libreoffice match"),
            "matched:LibreOffice"
        );
    }
}

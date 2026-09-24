//! Installs the global `paneru` API table into a Lua state.
//!
//! The command-issuing half (`paneru.run`, `paneru.window.*`,
//! `paneru.workspace.*`, `paneru.mouse.*`) comes from [`paneru_lua`], shared
//! with the client module so both hosts expose the same surface over a typed
//! [`Command`] dispatcher — here onto the command bus, there onto the daemon
//! socket.
//!
//! What's installed here is embedded-only: `paneru.on` (event handlers),
//! `paneru.bind` (keybinds), `paneru.flash`, `paneru.log`, and the `query*`
//! functions (named after the client's, but answering from the world
//! directly instead of over the socket).

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{IntoLua, Lua, LuaSerdeExt, Table, Value};
use tracing::{error, info};

use super::shared;
use crate::types::script_state::{ScriptStateWrite, WriteOutcome};

use super::convert::LuaEvent;
use super::runtime::{
    HandlerEntry, Outbox, SharedRegistry, from_lua_value, store_error, to_lua_value,
};
use super::world::DispatchWorld;
use crate::commands::Command;
use crate::config::{Config, config_from_lua, resolve_chord};
use crate::ecs::state::StateQueryKind;
use crate::types::windowset_lua::returned_ops;

/// One `paneru.exec` call: what to run, and where the answer goes.
struct ExecJob {
    program: String,
    args: Vec<String>,
    reply: async_channel::Sender<std::io::Result<ExecOutput>>,
}

/// What one finished `paneru.exec` command produced.
///
/// Deliberately not [`std::process::Output`]: the exit status can be genuinely
/// unknown while the output is still real, which an `ExitStatus` has no way to
/// say. See [`run_exec_job`].
struct ExecOutput {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs one command to completion, tolerating a child reaped out from under us.
///
/// [`std::process::Command::output`] spawns, drains the pipes and then waits —
/// and it is the wait that is not safe to trust here. paneru shares a process
/// with `AppKit`, and a framework that installs its own `SIGCHLD` disposition can
/// reap our child before we get to it; the wait then fails with `ECHILD` even
/// though the program ran perfectly. `output()` reports that as an error and
/// throws away everything the child wrote.
///
/// A failed `paneru.exec` raises a Lua error, and a Lua error aborts the whole
/// handler — so a lost reap does not merely lose an exit code, it stops the
/// rest of the handler from running. A `window_focused` handler that repaints a
/// bar stops repainting it, halfway through, on every event.
///
/// So spawn by hand and treat the pipes as the source of truth: they reach EOF
/// when the child's ends close, which is when it exits. Only the status is
/// allowed to come back unknown.
fn run_exec_job(program: &str, args: &[String]) -> std::io::Result<ExecOutput> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Both pipes have to be drained at once. Reading one to EOF while the child
    // fills the other's buffer deadlocks: it blocks on a write we are not
    // reading, we block on a read it will never reach.
    let mut errors = child.stderr.take();
    let draining = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(pipe) = errors.as_mut() {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    });
    let mut stdout = Vec::new();
    if let Some(pipe) = child.stdout.as_mut() {
        let _ = pipe.read_to_end(&mut stdout);
    }
    // A panic in the reader is not worth failing the command over; an empty
    // stderr is the same answer the old code gave when it could not read one.
    let stderr = draining.join().unwrap_or_default();

    // Both pipes are at EOF, so the child has already finished. All that is
    // left to collect is its status.
    let code = match child.wait() {
        Ok(status) => status.code(),
        // Already reaped elsewhere. The command ran and the output above is
        // real, so report it as run with an unknown status (`code = nil` in
        // Lua, exactly as a signal-killed child already reports) rather than
        // failing the handler.
        Err(err) if err.raw_os_error() == Some(libc::ECHILD) => None,
        Err(err) => return Err(err),
    };

    Ok(ExecOutput {
        code,
        stdout,
        stderr,
    })
}

/// How many `paneru.exec` commands may run at once when the machine's
/// parallelism cannot be determined. These are waits on other processes
/// rather than work, so a handful is enough that one slow command doesn't
/// hold up the rest.
const DEFAULT_EXEC_WORKERS: usize = 4;

/// The size of the `paneru.exec` worker pool: a fixed pool rather than a
/// thread per call, sized like every other thread pool in the process — from
/// the machine's available parallelism, the same figure bevy's task pools
/// use. Falls back to [`DEFAULT_EXEC_WORKERS`] on the platforms and cgroup
/// setups where that number is unavailable.
fn exec_workers() -> usize {
    std::thread::available_parallelism().map_or(DEFAULT_EXEC_WORKERS, std::num::NonZeroUsize::get)
}

/// Starts the pool that runs `paneru.exec` commands.
///
/// Jobs are taken from a shared queue, so two commands issued back to back
/// can finish out of order — a script that needs ordering should await the
/// first. Workers end when the returned sender is dropped (on reload), so a
/// reload gets a fresh pool rather than the old script's queued work.
fn spawn_exec_pool() -> async_channel::Sender<ExecJob> {
    let (jobs, queue) = async_channel::unbounded::<ExecJob>();
    for worker in 0..exec_workers() {
        let queue = queue.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("paneru-lua-exec-{worker}"))
            .spawn(move || {
                while let Ok(job) = queue.recv_blocking() {
                    let output = run_exec_job(&job.program, &job.args);
                    // The handler that asked may already be gone; its reply
                    // channel closing is not an error worth reporting.
                    let _ = job.reply.send_blocking(output);
                }
            });
        if let Err(err) = spawned {
            error!("could not start paneru.exec worker {worker}: {err}");
        }
    }
    jobs
}

/// How many times `paneru.state.mutate` may lose the compare-and-set race
/// before giving up; looping forever would wedge the handler's dispatch.
const MUTATE_ATTEMPTS: usize = 8;

/// Installs the `paneru` API into `lua`, wiring the Rust-backed functions to the
/// shared `outbox` (queued commands/flashes) and `registry` (registered handlers
/// and chords).
#[allow(clippy::too_many_lines)]
pub(super) fn install(
    lua: &Lua,
    outbox: &Rc<RefCell<Outbox>>,
    registry: &SharedRegistry,
    config_cell: &Rc<RefCell<Option<Config>>>,
    world: &Rc<DispatchWorld>,
) -> mlua::Result<()> {
    let paneru = lua.create_table()?;
    lua.globals().set("paneru", paneru.clone())?;

    // Queues the command onto the command bus; the primitive the shared API
    // is built on.
    let dispatch = {
        let outbox = Rc::clone(outbox);
        move |_: &Lua, command: Command| {
            outbox.borrow_mut().commands.push(command);
            Ok(true)
        }
    };
    shared::install(lua, &paneru, &(Rc::new(dispatch) as shared::Dispatch))?;
    // `cmd` is the embedded runtime's historical alias for `run`.
    let run: mlua::Function = paneru.get("run")?;
    paneru.set("cmd", run)?;

    install_query(lua, &paneru, world)?;
    install_script_state(lua, &paneru, world)?;

    // paneru.log(message) — emit a tracing log line.
    let log = lua.create_function(|_, message: String| {
        info!(target: "paneru::lua", "{message}");
        Ok(())
    })?;
    paneru.set("log", log)?;

    // paneru.flash(message[, duration]) — show an on-screen toast.
    let flash = {
        let outbox = Rc::clone(outbox);
        lua.create_function(move |_, (message, duration): (String, Option<f32>)| {
            outbox
                .borrow_mut()
                .flashes
                .push((message, duration.unwrap_or(2.0)));
            Ok(())
        })?
    };
    paneru.set("flash", flash)?;

    // paneru.exec(program[, args]) — run a program without holding the
    // interpreter while it runs.
    //
    // Async so the handler suspends and other handlers/world reads aren't
    // blocked behind it. A synchronous binding cannot be suspended — there is
    // no yield point inside a plain C function for mlua to resume from — so it
    // would stop every other handler until the child exits.
    let exec = {
        // Started lazily on first use; only ever touched from the Lua thread.
        let pool: Rc<RefCell<Option<async_channel::Sender<ExecJob>>>> = Rc::new(RefCell::new(None));
        lua.create_async_function(move |lua, (program, args): (String, Option<Vec<String>>)| {
            let jobs = pool
                .borrow_mut()
                .get_or_insert_with(spawn_exec_pool)
                .clone();
            async move {
                let (reply, answer) = async_channel::bounded(1);
                let job = ExecJob {
                    program,
                    args: args.unwrap_or_default(),
                    reply,
                };
                jobs.send(job)
                    .await
                    .map_err(|_| mlua::Error::RuntimeError("exec worker is gone".to_string()))?;
                let output = answer
                    .recv()
                    .await
                    .map_err(|_| mlua::Error::RuntimeError("exec worker is gone".to_string()))?
                    .map_err(|err| mlua::Error::RuntimeError(format!("exec: {err}")))?;

                let result = lua.create_table()?;
                result.set("code", output.code)?;
                result.set(
                    "stdout",
                    String::from_utf8_lossy(&output.stdout).into_owned(),
                )?;
                result.set(
                    "stderr",
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                )?;
                Ok(result)
            }
        })?
    };
    paneru.set("exec", exec)?;

    // paneru.on(event_name, [filter,] handler) — run `handler` on matching events.
    // Accepts either (name, handler), (name, filter_table, handler), or (name, filter_fn, handler).
    let on = {
        let registry = Rc::clone(registry);
        lua.create_function(move |lua, args: mlua::Variadic<Value>| {
            if args.len() < 2 || args.len() > 3 {
                return Err(mlua::Error::RuntimeError(
                    "paneru.on requires 2 or 3 arguments: (event_name, [filter,] handler)".into(),
                ));
            }
            let name = match &args[0] {
                Value::String(s) => s.to_str()?.to_string(),
                _ => {
                    return Err(mlua::Error::RuntimeError(
                        "paneru.on: expected event name string as 1st argument".into(),
                    ));
                }
            };
            if !LuaEvent::is_known(&name) {
                return Err(mlua::Error::RuntimeError(format!(
                    "paneru.on: unknown event '{name}'; known events are {}",
                    LuaEvent::NAMES.join(", ")
                )));
            }

            let (filter, handler) = if args.len() == 2 {
                let Value::Function(handler) = args[1].clone() else {
                    return Err(mlua::Error::RuntimeError(
                        "paneru.on: expected handler function as 2nd argument".into(),
                    ));
                };
                (None, handler)
            } else {
                let Value::Function(handler) = args[2].clone() else {
                    return Err(mlua::Error::RuntimeError(
                        "paneru.on: expected handler function as 3rd argument".into(),
                    ));
                };
                let filter_fn = match &args[1] {
                    Value::Table(table) => Some(shared::matcher(lua, table.clone())?),
                    Value::Function(f) => Some(f.clone()),
                    _ => {
                        return Err(mlua::Error::RuntimeError(
                            "paneru.on: expected table or function as filter".into(),
                        ));
                    }
                };
                (filter_fn, handler)
            };

            registry
                .borrow_mut()
                .handlers
                .entry(name)
                .or_default()
                .push(HandlerEntry { filter, handler });
            Ok(())
        })?
    };
    paneru.set("on", on)?;

    // paneru.bind(chord, handler) — register a keybind. `handler` is a Lua
    // function (receives a state snapshot) or a command string.
    let bind = {
        let registry = Rc::clone(registry);
        lua.create_function(move |_, (chord, handler): (String, Value)| {
            register_bind(&registry, &chord, handler)
        })?
    };
    paneru.set("bind", bind)?;

    // Initialize `paneru.config` with the built-in defaults so scripts can read
    // `paneru.config.options.*` (and other sections) even before or without a
    // `paneru.setup` call.
    let default_config = Config::defaults().unwrap_or_default();
    paneru.set("config", config_to_lua_table(lua, &default_config)?)?;

    // paneru.setup(table) — declare the whole configuration from Lua. Mirrors
    // the TOML sections; a `bindings` sub-table is desugared onto the same
    // path as `paneru.bind` and stripped before the rest is deserialized into
    // a `Config`. Updates `paneru.config` with the resolved configuration merged
    // with the user's table.
    let setup = {
        let registry = Rc::clone(registry);
        let config_cell = Rc::clone(config_cell);
        let paneru_table = paneru.clone();
        lua.create_function(move |lua, table: Table| {
            if let Some(bindings) = table.get::<Option<Table>>("bindings")? {
                for pair in bindings.pairs::<String, String>() {
                    let (command, chord) = pair?;
                    let handler = Value::String(lua.create_string(&command)?);
                    register_bind(&registry, &chord, handler)?;
                }
                table.set("bindings", Value::Nil)?;
            }
            let config = config_from_lua(lua, Value::Table(table.clone()))?;
            let resolved = config_to_lua_table(lua, &config)?;
            merge_lua_tables(&resolved, &table)?;
            paneru_table.set("config", resolved)?;
            *config_cell.borrow_mut() = Some(config);
            Ok(())
        })?
    };
    paneru.set("setup", setup)?;

    // paneru.windows(fn) — xmonad's `windows`: hand the window set to `fn` and
    // commit whatever it returns.
    //
    // Async because `fn` may itself query and fetching the set is a round
    // trip to the main thread; concurrent callers share one fetch via the
    // batch's cached copy.
    let windows = {
        let outbox = Rc::clone(outbox);
        let world = Rc::clone(world);
        lua.create_async_function(move |lua, transform: mlua::Function| {
            let outbox = Rc::clone(&outbox);
            let world = Rc::clone(&world);
            async move {
                let set = world.layout().await.map_err(mlua::Error::runtime)?;
                let window_set = lua.create_userdata((*set).clone())?;
                let returned: Value = transform.call_async(window_set).await?;
                let ops = returned_ops(&returned)?;
                if ops.is_empty() {
                    return Ok(false);
                }
                outbox.borrow_mut().commands.push(Command::Layout(ops));
                Ok(true)
            }
        })?
    };
    paneru.set("windows", windows)?;

    Ok(())
}

/// Registers one keybind into the shared registry: validates the handler is a
/// Lua function or a command string, resolves the chord to `(keycode,
/// modifiers)`, and records it for publishing to the event tap. Shared by
/// `paneru.bind` and the `bindings` sub-table of `paneru.setup`.
fn register_bind(registry: &SharedRegistry, chord: &str, handler: Value) -> mlua::Result<()> {
    match &handler {
        Value::Function(_) | Value::String(_) => {}
        other => {
            return Err(mlua::Error::RuntimeError(format!(
                "paneru.bind: handler must be a function or command string, got {}",
                other.type_name()
            )));
        }
    }
    let (code, modifiers) = resolve_chord(chord)
        .map_err(|err| mlua::Error::RuntimeError(format!("paneru.bind: {err}")))?;

    let mut registry = registry.borrow_mut();
    registry.binds.push(handler);
    let id = u32::try_from(registry.binds.len())
        .map_err(|_| mlua::Error::RuntimeError("paneru.bind: too many binds".into()))?;
    registry.keybinds.push((code, modifiers, id));
    Ok(())
}

/// Installs the state-query half of the API, matching the client module's
/// naming: `paneru.query(kind)` hands back the raw JSON string,
/// `paneru.query_json(kind)` the decoded table, and `query_state` /
/// `query_active` / `query_workspaces` / `query_on_screen` are fixed-kind
/// shorthands.
///
/// The world itself is only reachable while a dispatch is on the stack
/// (`super::LuaRuntime::with_query` installs the provider for exactly that
/// long), so calling one of these at script top level fails with an
/// explanation rather than returning stale data.
fn install_query(lua: &Lua, paneru: &mlua::Table, world: &Rc<DispatchWorld>) -> mlua::Result<()> {
    let raw = query_function(lua, world, None, true)?;
    paneru.set("query", raw)?;

    let json = query_function(lua, world, None, false)?;
    paneru.set("query_json", json)?;

    for (name, kind) in StateQueryKind::SHORTHANDS {
        let shorthand = query_function(lua, world, Some(kind), false)?;
        paneru.set(name, shorthand)?;
    }

    Ok(())
}

/// One `paneru.query*` entry point.
///
/// `fixed` is the kind for the shorthands, which take no argument; the general
/// forms take one and fall back to the full state document. `as_json` picks the
/// raw JSON string over the decoded table.
fn query_function(
    lua: &Lua,
    world: &Rc<DispatchWorld>,
    fixed: Option<StateQueryKind>,
    as_json: bool,
) -> mlua::Result<mlua::Function> {
    let world = Rc::clone(world);
    lua.create_async_function(move |lua, requested: Option<String>| {
        let world = Rc::clone(&world);
        async move {
            let kind = if let Some(kind) = fixed {
                kind
            } else {
                let token = requested
                    .as_deref()
                    .unwrap_or(StateQueryKind::State.token());
                // Rejected here as well as host-side so the error names the
                // valid kinds.
                StateQueryKind::parse(token).ok_or_else(|| {
                    mlua::Error::RuntimeError(format!(
                        "paneru.query: unknown kind '{token}'; expected one of {}",
                        StateQueryKind::tokens()
                    ))
                })?
            };
            let state = world
                .query_state()
                .await
                .map_err(|err| mlua::Error::RuntimeError(format!("paneru.query: {err}")))?;
            if as_json {
                state
                    .to_query_json(kind)
                    .map_err(mlua::Error::external)?
                    .into_lua(&lua)
            } else {
                let value = state.to_query_value(kind).map_err(mlua::Error::external)?;
                lua.to_value(&value)
            }
        }
    })
}

/// Installs `paneru.state`: a named store a script can keep values in,
/// surviving hot reloads and restarts (unlike a Lua global). A client can
/// also read and write the same store over the socket under the same names.
///
/// ```lua
/// paneru.state.get("pads.term")           -- the value, or nil
/// paneru.state.set("pads.term", 12345)    -- nil removes the key
/// paneru.state.mutate("count", function(n) return (n or 0) + 1 end)
/// ```
///
/// `mutate` reads, runs your function, and writes only if the value hasn't
/// changed since the read — retrying otherwise — so concurrent writers can't
/// lose an increment the way `get` then `set` can. Values must be
/// JSON-representable; functions, coroutines, and userdata are rejected
/// rather than silently dropped.
fn install_script_state(
    lua: &Lua,
    paneru: &mlua::Table,
    world: &Rc<DispatchWorld>,
) -> mlua::Result<()> {
    let state = lua.create_table()?;

    state.set("get", {
        let world = Rc::clone(world);
        lua.create_async_function(move |lua, key: String| {
            let world = Rc::clone(&world);
            async move {
                let store = world
                    .script_state()
                    .await
                    .map_err(|err| store_error("get", &err))?;
                to_lua_value(&lua, store.get(&key))
            }
        })?
    })?;

    // `set(key, nil)` removes the key, rather than storing a JSON null that
    // would still be present but read back as `nil`.
    state.set("set", {
        let world = Rc::clone(world);
        lua.create_async_function(move |lua, (key, value): (String, Value)| {
            let world = Rc::clone(&world);
            async move {
                let write = if value.is_nil() {
                    ScriptStateWrite::remove(key)
                } else {
                    ScriptStateWrite::set(key, from_lua_value(&lua, value, "set")?)
                };
                // The write has landed by the time this returns, so the cached
                // copy's revision has moved and the next read refreshes.
                world
                    .write_script_state(&write)
                    .await
                    .map(|_| ())
                    .map_err(|err| store_error("set", &err))
            }
        })?
    })?;

    // Read, transform, write, retrying against whatever the value moved to if
    // it changed underneath. `transform` runs here on the worker; only the
    // compare-and-set crosses to the main thread, which is what keeps this
    // atomic without the Lua function ever leaving this thread.
    state.set("mutate", {
        let world = Rc::clone(world);
        lua.create_async_function(move |lua, (key, transform): (String, mlua::Function)| {
            let world = Rc::clone(&world);
            async move {
                let store = world
                    .script_state()
                    .await
                    .map_err(|err| store_error("mutate", &err))?;
                let mut current = store.get(&key).cloned();

                for _ in 0..MUTATE_ATTEMPTS {
                    let next = {
                        let current = to_lua_value(&lua, current.as_ref())?;
                        // `call_async`, so a transform that queries suspends
                        // rather than wedging every other dispatch.
                        let returned: Value = transform.call_async(current).await?;
                        if returned.is_nil() {
                            None
                        } else {
                            Some(from_lua_value(&lua, returned, "mutate")?)
                        }
                    };
                    let write = ScriptStateWrite::compare_and_set(
                        key.clone(),
                        current.clone(),
                        next.clone(),
                    );
                    match world
                        .write_script_state(&write)
                        .await
                        .map_err(|err| store_error("mutate", &err))?
                    {
                        WriteOutcome::Applied { .. } => {
                            return to_lua_value(&lua, next.as_ref());
                        }
                        // The refusal carries what the key holds now, which is
                        // exactly what the next attempt has to transform.
                        WriteOutcome::Conflict {
                            current: overtaken, ..
                        } => current = overtaken,
                    }
                }

                Err(store_error(
                    "mutate",
                    &format!("'{key}' kept changing under it after {MUTATE_ATTEMPTS} attempts"),
                ))
            }
        })?
    })?;

    paneru.set("state", state)?;
    Ok(())
}

/// Builds a Lua table mirroring the configuration schema with all effective defaults
/// resolved from `config`.
///
/// Uses exhaustive destructuring of the option structs (without `..`) so adding a
/// new field to `MainOptions`, `PaddingOptions`, `SwipeOptions`, `GestureOptions`,
/// `ScrollOptions`, or `RestoreOptions` fails to compile until handled here.
#[allow(clippy::too_many_lines)]
fn config_to_lua_table(lua: &Lua, config: &Config) -> mlua::Result<Table> {
    use crate::config::{
        MainOptions, RestoreOptions, format_modifiers,
        padding::PaddingOptions,
        swipe::{GestureOptions, ScrollOptions, SwipeOptions},
    };

    let root = lua.create_table()?;
    root.set("default_workspaces", config.default_workspaces())?;

    let options = lua.create_table()?;
    let raw_opts = config.options();
    // Exhaustive destructure: adding any field to `MainOptions` triggers a
    // compile error until it is explicitly mapped onto `options` below.
    let MainOptions {
        focus_follows_mouse: _,
        mouse_follows_focus: _,
        horizontal_mouse_warp: _,
        horizontal_mouse_warp_offset: _,
        preset_column_widths: _,
        preset_stack_heights: _,
        animation_speed,
        auto_center: _,
        sliver_height: _,
        sliver_width: _,
        padding_top: _,
        padding_bottom: _,
        padding_left: _,
        padding_right: _,
        dim_inactive_windows: _,
        dim_inactive_color: _,
        border_active_window: _,
        border_color: _,
        border_opacity: _,
        border_width: _,
        border_radius: _,
        swipe_gesture_fingers: _,
        swipe_gesture_direction: _,
        continuous_swipe: _,
        swipe_sensitivity: _,
        swipe_deceleration: _,
        mouse_resize_modifier: _,
        menubar_height: _,
        window_hidden_ratio: _,
        window_resize_cycle: _,
        reap_empty_workspaces: _,
        disable_native_tabs: _,
        virtual_workspace_animations: _,
        insert_windows_mid_strip: _,
        create_virtual_workspace_automatically: _,
    } = &raw_opts;

    options.set("focus_follows_mouse", config.focus_follows_mouse())?;
    options.set("mouse_follows_focus", config.mouse_follows_focus())?;
    options.set("horizontal_mouse_warp", config.horizontal_mouse_warp())?;
    options.set(
        "horizontal_mouse_warp_offset",
        config.horizontal_mouse_warp_offset(),
    )?;
    options.set("preset_column_widths", config.preset_column_widths())?;
    options.set("preset_stack_heights", config.preset_stack_heights())?;
    options.set("animation_speed", *animation_speed)?;
    options.set("auto_center", config.auto_center())?;
    options.set("sliver_height", config.sliver_height())?;
    options.set("sliver_width", config.sliver_width())?;
    options.set(
        "mouse_resize_modifier",
        config.mouse_resize_modifier().map(format_modifiers),
    )?;
    options.set("menubar_height", config.menubar_height())?;
    options.set("window_hidden_ratio", config.window_hidden_ratio())?;
    options.set("window_resize_cycle", config.window_resize_cycle())?;
    options.set("reap_empty_workspaces", config.reap_empty_workspaces())?;
    options.set("disable_native_tabs", !config.native_tabs_enabled())?;
    options.set(
        "virtual_workspace_animations",
        config.virtual_workspace_animations(),
    )?;
    options.set(
        "insert_windows_mid_strip",
        config.insert_windows_mid_strip(),
    )?;
    options.set(
        "create_virtual_workspace_automatically",
        config.create_workspace_automatically(),
    )?;
    root.set("options", options)?;

    let PaddingOptions {
        top: _,
        bottom: _,
        left: _,
        right: _,
    } = PaddingOptions::default();
    let padding = lua.create_table()?;
    let (top, right, bottom, left) = config.edge_padding();
    padding.set("top", top)?;
    padding.set("right", right)?;
    padding.set("bottom", bottom)?;
    padding.set("left", left)?;
    root.set("padding", padding)?;

    let SwipeOptions {
        sensitivity: _,
        deceleration: _,
        continuous: _,
        gesture: _,
        scroll: _,
    } = SwipeOptions::default();
    let swipe = lua.create_table()?;
    swipe.set("sensitivity", config.swipe_sensitivity())?;
    swipe.set("deceleration", config.swipe_deceleration())?;
    swipe.set("continuous", config.continuous_swipe())?;

    let GestureOptions {
        fingers_count: _,
        direction: _,
        vertical: _,
    } = GestureOptions::default();
    let gesture = lua.create_table()?;
    gesture.set("fingers_count", config.swipe_gesture_fingers())?;
    let direction_str = match config.swipe_gesture_direction() {
        crate::config::swipe::SwipeGestureDirection::Natural => "Natural",
        crate::config::swipe::SwipeGestureDirection::Reversed => "Reversed",
    };
    gesture.set("direction", direction_str)?;
    gesture.set("vertical", config.swipe_vertical())?;
    swipe.set("gesture", gesture)?;

    let ScrollOptions {
        window_step: _,
        modifier: _,
        vertical_modifier: _,
    } = ScrollOptions::default();
    let scroll = lua.create_table()?;
    scroll.set("window_step", config.swipe_scroll_window_step())?;
    scroll.set("modifier", format_modifiers(config.swipe_scroll_modifier()))?;
    scroll.set(
        "vertical_modifier",
        config
            .swipe_scroll_vertical_modifier()
            .map(format_modifiers),
    )?;
    swipe.set("scroll", scroll)?;
    root.set("swipe", swipe)?;

    let decorations = lua.create_table()?;
    decorations.set("workspace_menu_status", config.workspace_menu_status())?;
    decorations.set("workspace_popup_status", config.workspace_popup_status())?;

    let active = lua.create_table()?;
    let border = lua.create_table()?;
    border.set("enabled", config.border_active_window())?;
    border.set("opacity", config.border_opacity())?;
    border.set("width", config.border_width())?;
    border.set("color", "#FFFFFF")?;
    active.set("border", border)?;
    decorations.set("active", active)?;

    let inactive = lua.create_table()?;
    let dim = lua.create_table()?;
    dim.set("opacity", config.dim_inactive_opacity())?;
    dim.set("color", "#000000")?;
    inactive.set("dim", dim)?;
    decorations.set("inactive", inactive)?;
    root.set("decorations", decorations)?;

    let RestoreOptions {
        enabled: _,
        startup_grace_ms: _,
        missing_windows: _,
    } = RestoreOptions::default();
    let restore = lua.create_table()?;
    restore.set("enabled", config.restore_enabled())?;
    restore.set(
        "startup_grace_ms",
        u64::try_from(config.restore_startup_grace().as_millis()).unwrap_or(2000),
    )?;
    restore.set("missing_windows", "ignore")?;
    root.set("restore", restore)?;

    let windows = lua.create_table()?;
    root.set("windows", windows)?;

    Ok(root)
}

fn is_lua_array(table: &Table) -> bool {
    table.raw_len() > 0
}

/// Deep-merges map tables from `src` into `dst`, replacing array tables and
/// scalar values directly.
fn merge_lua_tables(dst: &Table, src: &Table) -> mlua::Result<()> {
    for pair in src.pairs::<Value, Value>() {
        let (key, src_val) = pair?;
        if let Value::Table(src_sub) = &src_val
            && !is_lua_array(src_sub)
            && let Value::Table(dst_sub) = dst.get::<Value>(key.clone())?
            && !is_lua_array(&dst_sub)
        {
            merge_lua_tables(&dst_sub, src_sub)?;
        } else {
            dst.set(key, src_val)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        MainOptions, RestoreOptions,
        padding::PaddingOptions,
        swipe::{GestureOptions, ScrollOptions, SwipeOptions},
    };

    /// Legacy top-level keys on `MainOptions` that have moved to dedicated
    /// `[padding]`, `[decorations]`, and `[swipe]` sub-tables.
    const LEGACY_MAIN_OPTION_KEYS: &[&str] = &[
        "padding_top",
        "padding_bottom",
        "padding_left",
        "padding_right",
        "dim_inactive_windows",
        "dim_inactive_color",
        "border_active_window",
        "border_color",
        "border_opacity",
        "border_width",
        "border_radius",
        "swipe_gesture_fingers",
        "swipe_gesture_direction",
        "continuous_swipe",
        "swipe_sensitivity",
        "swipe_deceleration",
    ];

    fn struct_field_names<T: serde::Serialize + Default>() -> Vec<String> {
        let serde_json::Value::Object(map) =
            serde_json::to_value(T::default()).expect("struct should serialize to JSON object")
        else {
            panic!("expected JSON object");
        };
        map.keys().cloned().collect()
    }

    #[test]
    fn config_to_lua_table_covers_every_struct_field() {
        // Build a config where optional fields without built-in scalar defaults
        // are populated, so every mapped key in the Lua table is non-nil.
        let config = Config::try_from(
            r#"
            default_workspaces = 2

            [options]
            horizontal_mouse_warp = 10
            animation_speed = 12.0
            mouse_resize_modifier = "alt"
            menubar_height = 24

            [swipe.gesture]
            fingers_count = 3

            [swipe.scroll]
            modifier = "alt"
            vertical_modifier = "shift"
            "#,
        )
        .expect("valid test config");

        let lua = Lua::new();
        let root = config_to_lua_table(&lua, &config).expect("config_to_lua_table should succeed");

        let options_table: Table = root.get("options").unwrap();
        for field in struct_field_names::<MainOptions>() {
            if LEGACY_MAIN_OPTION_KEYS.contains(&field.as_str()) {
                continue;
            }
            let val: Value = options_table.get(field.as_str()).unwrap();
            assert!(
                !val.is_nil(),
                "MainOptions field '{field}' is missing from paneru.config.options! \
                 Did you add a new option to MainOptions and forget to set it in config_to_lua_table?"
            );
        }

        let padding_table: Table = root.get("padding").unwrap();
        for field in struct_field_names::<PaddingOptions>() {
            let val: Value = padding_table.get(field.as_str()).unwrap();
            assert!(
                !val.is_nil(),
                "PaddingOptions field '{field}' is missing from paneru.config.padding!"
            );
        }

        let swipe_table: Table = root.get("swipe").unwrap();
        for field in struct_field_names::<SwipeOptions>() {
            let val: Value = swipe_table.get(field.as_str()).unwrap();
            assert!(
                !val.is_nil(),
                "SwipeOptions field '{field}' is missing from paneru.config.swipe!"
            );
        }

        let gesture_table: Table = swipe_table.get("gesture").unwrap();
        for field in struct_field_names::<GestureOptions>() {
            let val: Value = gesture_table.get(field.as_str()).unwrap();
            assert!(
                !val.is_nil(),
                "GestureOptions field '{field}' is missing from paneru.config.swipe.gesture!"
            );
        }

        let scroll_table: Table = swipe_table.get("scroll").unwrap();
        for field in struct_field_names::<ScrollOptions>() {
            let val: Value = scroll_table.get(field.as_str()).unwrap();
            assert!(
                !val.is_nil(),
                "ScrollOptions field '{field}' is missing from paneru.config.swipe.scroll!"
            );
        }

        let restore_table: Table = root.get("restore").unwrap();
        for field in struct_field_names::<RestoreOptions>() {
            let val: Value = restore_table.get(field.as_str()).unwrap();
            assert!(
                !val.is_nil(),
                "RestoreOptions field '{field}' is missing from paneru.config.restore!"
            );
        }
    }
}

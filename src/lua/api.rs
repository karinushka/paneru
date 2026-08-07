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

use paneru_lua as shared;
use paneru_shared_types::script_state::{ScriptStateWrite, WriteOutcome};

use super::convert::LuaEvent;
use super::runtime::{Outbox, SharedRegistry, from_lua_value, store_error, to_lua_value};
use super::world::DispatchWorld;
use crate::commands::Command;
use crate::config::{Config, config_from_lua, resolve_chord};
use crate::ecs::state::StateQueryKind;
use paneru_shared_types::windowset_lua::returned_ops;

/// One `paneru.exec` call: what to run, and where the answer goes.
struct ExecJob {
    program: String,
    args: Vec<String>,
    reply: async_channel::Sender<std::io::Result<std::process::Output>>,
}

/// How many `paneru.exec` commands may run at once. A fixed worker pool
/// rather than a thread per call; four is enough that one slow command
/// doesn't hold up the rest, since these are waits on other processes, not
/// work.
const EXEC_WORKERS: usize = 4;

/// Starts the pool that runs `paneru.exec` commands.
///
/// Jobs are taken from a shared queue, so two commands issued back to back
/// can finish out of order — a script that needs ordering should await the
/// first. Workers end when the returned sender is dropped (on reload), so a
/// reload gets a fresh pool rather than the old script's queued work.
fn spawn_exec_pool() -> async_channel::Sender<ExecJob> {
    let (jobs, queue) = async_channel::unbounded::<ExecJob>();
    for worker in 0..EXEC_WORKERS {
        let queue = queue.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("paneru-lua-exec-{worker}"))
            .spawn(move || {
                while let Ok(job) = queue.recv_blocking() {
                    let output = std::process::Command::new(&job.program)
                        .args(&job.args)
                        .output();
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
                result.set("code", output.status.code())?;
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

    // paneru.on(event_name, handler) — run `handler` on every matching event.
    // Unknown names are rejected here rather than silently never firing.
    let on = {
        let registry = Rc::clone(registry);
        lua.create_function(move |_, (name, handler): (String, mlua::Function)| {
            if !LuaEvent::is_known(&name) {
                return Err(mlua::Error::RuntimeError(format!(
                    "paneru.on: unknown event '{name}'; known events are {}",
                    LuaEvent::NAMES.join(", ")
                )));
            }
            registry
                .borrow_mut()
                .handlers
                .entry(name)
                .or_default()
                .push(handler);
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

    // paneru.setup(table) — declare the whole configuration from Lua. The table
    // mirrors the TOML sections one-for-one. A `bindings` sub-table (command
    // string = chord string) is desugared onto the same path as `paneru.bind`
    // and stripped out before the rest is deserialized into a `Config`.
    let setup = {
        let registry = Rc::clone(registry);
        let config_cell = Rc::clone(config_cell);
        lua.create_function(move |lua, table: Table| {
            if let Some(bindings) = table.get::<Option<Table>>("bindings")? {
                for pair in bindings.pairs::<String, String>() {
                    let (command, chord) = pair?;
                    let handler = Value::String(lua.create_string(&command)?);
                    register_bind(&registry, &chord, handler)?;
                }
                table.set("bindings", Value::Nil)?;
            }
            let config = config_from_lua(lua, Value::Table(table))?;
            *config_cell.borrow_mut() = Some(config);
            Ok(())
        })?
    };
    paneru.set("setup", setup)?;

    // paneru.windows(fn) — xmonad's `windows`: hand the window set to `fn` and
    // commit whatever it hands back. The same contract as a bind handler, for
    // use partway through one.
    //
    // Async because `fn` may itself query, and because fetching the set is a
    // round trip to the main thread. It reads the batch's shared copy, so
    // calling this from several concurrent handlers costs one fetch.
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
/// spelling exactly: `paneru.query(kind)` hands back the raw JSON string,
/// `paneru.query_json(kind)` the decoded table, and `query_state` /
/// `query_active` / `query_workspaces` / `query_on_screen` are the fixed-kind
/// shorthands. A script therefore reads state the same way whether it runs
/// inside the daemon or in a client process.
///
/// The functions themselves only find the provider and unpack its answer; what
/// they cannot do is reach the world, which is not accessible outside a
/// dispatching system. `super::LuaRuntime::with_query` installs a provider for
/// exactly as long as a callback is on the stack, so calling one of these at
/// script top level fails with an explanation rather than stale data.
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

/// Installs `paneru.state`: the named store a script keeps things in.
///
/// A Lua global is the obvious place to put something a handler wants to
/// remember, and the wrong one — a hot reload builds a whole new interpreter,
/// so what is kept there is wiped every time the script is saved. What goes in
/// here survives that and a restart, and a client can read and write the same
/// store over the socket under the same names.
///
/// Three calls:
///
/// ```lua
/// paneru.state.get("pads.term")           -- the value, or nil
/// paneru.state.set("pads.term", 12345)    -- nil removes the key
/// paneru.state.mutate("count", function(n) return (n or 0) + 1 end)
/// ```
///
/// `mutate` is the one to reach for when the new value depends on the old.
/// It reads, runs your function, and writes only if the value is still what it
/// read — otherwise it runs your function again against what the value became.
/// Two handlers, or a handler and a client, incrementing the same counter
/// therefore cannot lose an increment the way a `get` followed by a `set` can.
/// It returns what it stored.
///
/// Values are anything that maps onto JSON: strings, numbers, booleans, and
/// tables of those. A function, a coroutine or a userdata cannot be stored, and
/// says so rather than being silently dropped.
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

    // `set(key, nil)` removes: the Lua way of spelling "there is nothing here"
    // is the absence of a value, and storing a JSON null instead would leave a
    // key that reads back as `nil` but is still there.
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

    // Read, transform, write — and if the value moved under it in between, do it
    // again against what it moved to. `transform` is the script's own function,
    // so it runs here on the worker; only the compare-and-set crosses to the
    // main thread. That is what makes the whole thing atomic without a Lua
    // function ever having to leave this thread: a competing write cannot land
    // inside the read-modify-write, it can only lose the compare and send us
    // round again.
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

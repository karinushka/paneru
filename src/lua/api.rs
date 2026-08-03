//! Installs the global `paneru` API table into a Lua state.
//!
//! The command-issuing half of the API (`paneru.run`, `paneru.window.*`,
//! `paneru.workspace.*`, `paneru.mouse.*`) is not defined here: it comes from
//! [`paneru_lua`], the crate that also builds the loadable client module, so
//! both hosts install the same surface. Both hosts hand it a dispatcher taking a typed
//! [`Command`] — here it goes onto the command bus, there onto the daemon
//! socket — so a script sees one identical API either way.
//!
//! What is installed here is the embedded-only half: `paneru.on` (event
//! handlers), `paneru.bind` (keybinds), `paneru.flash` and `paneru.log`. Their
//! callbacks are kept in a Rust-side [`Registry`] rather than in Lua globals, so
//! there is no scaffolding script to keep in sync with the Rust that calls it.
//!
//! The `query*` functions are named after the client's, but answer from the
//! world directly instead of over the socket — see [`provider`].

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Table, Value};
use regex::Regex;
use tracing::info;

use paneru_lua as shared;

use super::convert::LuaEvent;
use super::runtime::{Outbox, SharedRegistry};
use super::windowset::LuaWindowSet;
use crate::commands::Command;
use crate::config::{Config, config_from_lua, resolve_chord};
use crate::ecs::state::StateQueryKind;

/// Registry key holding the short-lived function that answers a query against
/// the live world. Kept in the Lua registry rather than on the `paneru` table
/// so a script can neither see nor overwrite it.
pub(super) const QUERY_PROVIDER: &str = "paneru.query_provider";

/// Installs the `paneru` API into `lua`, wiring the Rust-backed functions to the
/// shared `outbox` (queued commands/flashes) and `registry` (registered handlers
/// and chords).
// One block per `paneru.*` function, kept flat so adding one is a local change
// rather than a hunt across helpers.
#[allow(clippy::too_many_lines)]
pub(super) fn install(
    lua: &Lua,
    outbox: &Rc<RefCell<Outbox>>,
    registry: &SharedRegistry,
    config_cell: &Rc<RefCell<Option<Config>>>,
) -> mlua::Result<()> {
    let paneru = lua.create_table()?;
    lua.globals().set("paneru", paneru.clone())?;

    // The one primitive the shared API is built on: queue the command it built
    // for the command bus.
    let dispatch = {
        let outbox = Rc::clone(outbox);
        move |_: &Lua, command: &Command| {
            outbox.borrow_mut().commands.push(command.clone());
            Ok(true)
        }
    };
    shared::install(lua, &paneru, &(Rc::new(dispatch) as shared::Dispatch))?;
    // `cmd` is the embedded runtime's historical alias for `run`.
    let run: mlua::Function = paneru.get("run")?;
    paneru.set("cmd", run)?;

    install_query(lua, &paneru)?;

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
    let windows = {
        let outbox = Rc::clone(outbox);
        lua.create_function(move |lua, transform: mlua::Function| {
            let window_set = lua.create_userdata(LuaWindowSet::lazy())?;
            let returned: Value = transform.call(window_set)?;
            let ops = match &returned {
                Value::Nil => Vec::new(),
                Value::UserData(data) => data.borrow::<LuaWindowSet>()?.ops(),
                other => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "paneru.windows: expected a window set back, got {}",
                        other.type_name()
                    )));
                }
            };
            if ops.is_empty() {
                return Ok(false);
            }
            outbox.borrow_mut().commands.push(Command::Layout(ops));
            Ok(true)
        })?
    };
    paneru.set("windows", windows)?;

    // paneru.match{ app = …, bundle = …, title = …, floating = …, managed = … }
    // — builds a predicate over window records, for `ws:find`/`ws:filter`.
    // `app`, `bundle` and `title` are regular expressions, compiled here so a
    // bad pattern is an error at startup rather than a handler that never
    // matches anything.
    let matcher = lua.create_function(|lua, spec: mlua::Table| {
        let pattern = |field: &str| -> mlua::Result<Option<Regex>> {
            let Some(pattern) = spec.get::<Option<String>>(field)? else {
                return Ok(None);
            };
            Regex::new(&pattern)
                .map(Some)
                .map_err(|err| mlua::Error::RuntimeError(format!("paneru.match: {field}: {err}")))
        };
        let (app, bundle, title) = (pattern("app")?, pattern("bundle")?, pattern("title")?);
        let floating: Option<bool> = spec.get("floating")?;
        let managed: Option<bool> = spec.get("managed")?;

        for entry in spec.pairs::<String, Value>() {
            let (key, _) = entry?;
            if !matches!(
                key.as_str(),
                "app" | "bundle" | "title" | "floating" | "managed"
            ) {
                return Err(mlua::Error::RuntimeError(format!(
                    "paneru.match: unknown field '{key}'"
                )));
            }
        }

        lua.create_function(move |_, window: mlua::Table| {
            let matches = |regex: &Option<Regex>, field: &str| -> mlua::Result<bool> {
                let Some(regex) = regex else {
                    return Ok(true);
                };
                Ok(regex.is_match(&window.get::<String>(field)?))
            };
            let flag = |want: Option<bool>, field: &str| -> mlua::Result<bool> {
                match want {
                    Some(want) => Ok(window.get::<bool>(field)? == want),
                    None => Ok(true),
                }
            };
            Ok(matches(&app, "app_name")?
                && matches(&bundle, "bundle_id")?
                && matches(&title, "title")?
                && flag(floating, "floating")?
                && flag(managed, "managed")?)
        })
    })?;
    paneru.set("match", matcher)?;

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
fn install_query(lua: &Lua, paneru: &mlua::Table) -> mlua::Result<()> {
    let query_raw = lua
        .create_function(|lua, kind: Option<String>| query::<String>(lua, kind.as_deref(), true))?;
    paneru.set("query", query_raw)?;

    let query_json = lua
        .create_function(|lua, kind: Option<String>| query::<Value>(lua, kind.as_deref(), false))?;
    paneru.set("query_json", query_json)?;

    for (name, kind) in StateQueryKind::SHORTHANDS {
        let shorthand =
            lua.create_function(move |lua, ()| query::<Value>(lua, Some(kind.token()), false))?;
        paneru.set(name, shorthand)?;
    }

    Ok(())
}

/// Runs one query through the currently installed provider. `as_json` picks
/// the raw JSON string over the decoded table, and `R` is what that yields.
fn query<R: mlua::FromLuaMulti>(lua: &Lua, kind: Option<&str>, as_json: bool) -> mlua::Result<R> {
    let kind = kind.unwrap_or_else(|| StateQueryKind::State.token());
    // Rejected here as well as host-side so the error names the valid kinds.
    if StateQueryKind::parse(kind).is_none() {
        return Err(mlua::Error::RuntimeError(format!(
            "paneru.query: unknown kind '{kind}'; expected one of {}",
            StateQueryKind::tokens()
        )));
    }
    provider(lua)?.call((kind.to_string(), as_json))
}

/// The provider installed for the duration of the current callback, or an
/// error explaining that there is no world to query from here.
fn provider(lua: &Lua) -> mlua::Result<mlua::Function> {
    lua.named_registry_value::<Option<mlua::Function>>(QUERY_PROVIDER)?
        .ok_or_else(|| {
            mlua::Error::RuntimeError(
                "paneru.query is only available inside a paneru.on handler or a paneru.bind \
                 callback"
                    .into(),
            )
        })
}

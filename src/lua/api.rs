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

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Value};
use tracing::info;

use paneru_lua as shared;

use super::convert::LuaEvent;
use super::{Outbox, SharedRegistry};
use crate::commands::Command;
use crate::config::resolve_chord;

/// Installs the `paneru` API into `lua`, wiring the Rust-backed functions to the
/// shared `outbox` (queued commands/flashes) and `registry` (registered handlers
/// and chords).
pub(super) fn install(
    lua: &Lua,
    outbox: &Rc<RefCell<Outbox>>,
    registry: &SharedRegistry,
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
            match &handler {
                Value::Function(_) | Value::String(_) => {}
                other => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "paneru.bind: handler must be a function or command string, got {}",
                        other.type_name()
                    )));
                }
            }
            let (code, modifiers) = resolve_chord(&chord)
                .map_err(|err| mlua::Error::RuntimeError(format!("paneru.bind: {err}")))?;

            let mut registry = registry.borrow_mut();
            registry.binds.push(handler);
            let id = u32::try_from(registry.binds.len())
                .map_err(|_| mlua::Error::RuntimeError("paneru.bind: too many binds".into()))?;
            registry.keybinds.push((code, modifiers, id));
            Ok(())
        })?
    };
    paneru.set("bind", bind)?;

    Ok(())
}

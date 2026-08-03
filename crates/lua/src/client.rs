//! The client half of the API: talking to a running daemon over its Unix
//! socket.
//!
//! [`module`] builds the complete table that `require("paneru")` returns — the
//! shared typed API from [`crate::install`], dispatching over the socket, plus
//! the client-only `query_*` / `subscribe` / socket-path helpers. The loadable
//! `module` feature's `luaopen_paneru` is then a one-liner returning this table.
//!
//! The socket path defaults to `/tmp/paneru.socket`, can be overridden with the
//! `PANERU_SOCKET` environment variable, or set at runtime via
//! `paneru.set_socket_path("/path/to.socket")`.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::{LazyLock, Mutex};

use mlua::prelude::*;
use paneru_shared_types::commands::Command;
use paneru_shared_types::state::StateQueryKind;

/// Default daemon socket path (matches `CommandReader::SOCKET_PATH`).
const DEFAULT_SOCKET: &str = "/tmp/paneru.socket";

/// The active socket path, seeded from `$PANERU_SOCKET` and mutable via
/// `set_socket_path`.
static SOCKET_PATH: LazyLock<Mutex<String>> = LazyLock::new(|| {
    Mutex::new(std::env::var("PANERU_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_string()))
});

fn current_socket() -> String {
    SOCKET_PATH
        .lock()
        .map_or_else(|_| DEFAULT_SOCKET.to_string(), |guard| guard.clone())
}

/// Encodes argv into a Paneru socket frame: `<u32 le len><arg\0 arg\0 ...>`.
fn encode(argv: &[String]) -> Vec<u8> {
    let mut payload = Vec::new();
    for arg in argv {
        payload.extend_from_slice(arg.as_bytes());
        payload.push(0);
    }
    let len = u32::try_from(payload.len()).unwrap_or(0);
    let mut frame = len.to_le_bytes().to_vec();
    frame.extend_from_slice(&payload);
    frame
}

/// Connects to the daemon and writes `argv`, returning the open stream.
fn send(argv: &[String]) -> LuaResult<UnixStream> {
    let mut stream = UnixStream::connect(current_socket()).map_err(LuaError::external)?;
    stream
        .write_all(&encode(argv))
        .map_err(LuaError::external)?;
    Ok(stream)
}

/// The primitive the shared API is built on: encode the command and send it to
/// the daemon (fire-and-forget).
// Takes `Command` by value to match the shared `crate::Dispatch` closure type,
// which every verb closure in `lib.rs` also has to satisfy.
#[allow(clippy::needless_pass_by_value)]
fn dispatch(_: &Lua, command: Command) -> LuaResult<bool> {
    let argv = command.to_argv().ok_or_else(|| {
        LuaError::RuntimeError(format!("{command:?} cannot be sent to the daemon"))
    })?;
    let _stream = send(&argv)?;
    Ok(true)
}

/// `paneru.query(kind)` — run a state query and return the raw JSON string.
fn query(_: &Lua, kind: Option<String>) -> LuaResult<String> {
    let kind = kind.unwrap_or_else(|| "state".to_string());
    let mut stream = send(&["query".to_string(), kind, "--json".to_string()])?;
    let mut out = String::new();
    stream
        .read_to_string(&mut out)
        .map_err(LuaError::external)?;
    Ok(out)
}

/// `paneru.query_json(kind)` — like `query` but decoded into a Lua value.
fn query_json(lua: &Lua, kind: Option<String>) -> LuaResult<LuaValue> {
    let raw = query(lua, kind)?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(LuaError::external)?;
    lua.to_value(&value)
}

/// Runs one script-state request and decodes the daemon's JSON answer.
///
/// The daemon answers every one of these with a single object: the value asked
/// for, or `{"error": …}`, which becomes a Lua error here so a client sees a
/// failed write as a failure rather than as a silent no-op.
fn script_state(argv: &[String]) -> LuaResult<serde_json::Value> {
    let full: Vec<String> = std::iter::once("state".to_string())
        .chain(argv.iter().cloned())
        .collect();
    let mut stream = send(&full)?;
    let mut out = String::new();
    stream
        .read_to_string(&mut out)
        .map_err(LuaError::external)?;
    let reply: serde_json::Value = serde_json::from_str(out.trim()).map_err(LuaError::external)?;
    if let Some(error) = reply.get("error").and_then(serde_json::Value::as_str) {
        return Err(LuaError::RuntimeError(format!("paneru.state: {error}")));
    }
    Ok(reply)
}

/// How the wire spells "there is no value here": absent in a compare, a
/// removal in a write. Unambiguous next to JSON, where a string is quoted.
const ABSENT: &str = "-";

/// Reads the current value of `key`.
fn state_get(key: &str) -> LuaResult<Option<serde_json::Value>> {
    let reply = script_state(&["get".to_string(), key.to_string()])?;
    Ok(match reply.get("value") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(value.clone()),
    })
}

/// Encodes an optional value for the wire.
fn wire(value: Option<&serde_json::Value>) -> String {
    value.map_or_else(|| ABSENT.to_string(), serde_json::Value::to_string)
}

/// Builds the `paneru.state` table: the same store, under the same names, as
/// the embedded runtime gives a script.
///
/// The one difference worth knowing is cost. Each call here is a round-trip to
/// the daemon, where the embedded runtime reads a copy it already has; the
/// values are the same either way, and so is `mutate`'s guarantee — the daemon
/// is the one deciding whether a write still matches what it was read as, so a
/// client and a script contending on a key resolve against the same store.
fn state_table(lua: &Lua) -> LuaResult<LuaTable> {
    let state = lua.create_table()?;

    state.set(
        "get",
        lua.create_function(|lua, key: String| match state_get(&key)? {
            Some(value) => lua.to_value(&value),
            None => Ok(LuaValue::Nil),
        })?,
    )?;

    // `set(key, nil)` removes, matching the embedded spelling.
    state.set(
        "set",
        lua.create_function(|lua, (key, value): (String, LuaValue)| {
            if value.is_nil() {
                script_state(&["remove".to_string(), key])?;
                return Ok(());
            }
            let value: serde_json::Value = lua.from_value(value)?;
            script_state(&["set".to_string(), key, value.to_string()])?;
            Ok(())
        })?,
    )?;

    // Read, transform, write — retried against the current value whenever the
    // daemon reports the key moved in between.
    state.set(
        "mutate",
        lua.create_function(|lua, (key, transform): (String, LuaFunction)| {
            /// How many times to lose the race before giving up, matching the
            /// embedded runtime.
            const ATTEMPTS: usize = 8;

            let mut current = state_get(&key)?;
            for _ in 0..ATTEMPTS {
                let old = match &current {
                    Some(value) => lua.to_value(value)?,
                    None => LuaValue::Nil,
                };
                let returned: LuaValue = transform.call(old)?;
                let next: Option<serde_json::Value> = if returned.is_nil() {
                    None
                } else {
                    Some(lua.from_value(returned)?)
                };

                let reply = script_state(&[
                    "cas".to_string(),
                    key.clone(),
                    wire(current.as_ref()),
                    wire(next.as_ref()),
                ])?;
                if reply.get("conflict").is_none() {
                    return match next {
                        Some(value) => lua.to_value(&value),
                        None => Ok(LuaValue::Nil),
                    };
                }
                current = match reply.get("current") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(value) => Some(value.clone()),
                };
            }
            Err(LuaError::RuntimeError(format!(
                "paneru.state.mutate: '{key}' kept changing under it after {ATTEMPTS} attempts"
            )))
        })?,
    )?;

    Ok(state)
}

/// Reads the `event` argument to `subscribe`: `nil` (every event), a single
/// event name, or a table listing several.
fn read_events(value: &LuaValue) -> LuaResult<Option<Vec<String>>> {
    match value {
        LuaValue::Nil => Ok(None),
        LuaValue::String(name) => Ok(Some(vec![name.to_str()?.to_string()])),
        LuaValue::Table(names) => Ok(Some(
            names
                .clone()
                .sequence_values::<String>()
                .collect::<LuaResult<_>>()?,
        )),
        other => Err(LuaError::RuntimeError(format!(
            "event must be a string, a table of strings, or nil, got {}",
            other.type_name()
        ))),
    }
}

/// `paneru.subscribe(event, callback[, opts])` — stream events matching
/// `event` to `callback`. Blocks until the daemon closes the connection, so
/// run it in a dedicated process. `event` is the event name to filter on
/// (e.g. `"window_focused"`), a table of several names, or `nil` for every
/// event. Each event is a decoded Lua table unless `opts.decode == false`
/// (then the raw JSON line string). On a decode error the callback receives
/// `(line, "decode error")` regardless of the event filter.
fn subscribe(
    lua: &Lua,
    (event, callback, opts): (LuaValue, LuaFunction, Option<LuaTable>),
) -> LuaResult<bool> {
    let events = read_events(&event)?;
    let decode = opts
        .as_ref()
        .and_then(|opts| opts.get::<Option<bool>>("decode").ok().flatten())
        .unwrap_or(true);

    let stream = send(&["subscribe".to_string(), "--json".to_string()])?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.map_err(LuaError::external)?;
        if line.is_empty() {
            continue;
        }

        let parsed = serde_json::from_str::<serde_json::Value>(&line);
        let Ok(parsed) = parsed else {
            callback.call::<()>((line, "decode error".to_string()))?;
            continue;
        };
        if let Some(events) = &events {
            let name = parsed.get("event").and_then(serde_json::Value::as_str);
            let matches = name.is_some_and(|name| events.iter().any(|event| event == name));
            if !matches {
                continue;
            }
        }
        if decode {
            let value = lua.to_value(&parsed)?;
            callback.call::<()>(value)?;
        } else {
            callback.call::<()>(line)?;
        }
    }
    Ok(true)
}

/// `paneru.set_socket_path(path)` — override the daemon socket path.
// Signature is fixed by mlua's `create_function` contract.
#[allow(clippy::unnecessary_wraps)]
fn set_socket_path(_: &Lua, path: String) -> LuaResult<()> {
    if let Ok(mut guard) = SOCKET_PATH.lock() {
        *guard = path;
    }
    Ok(())
}

/// `paneru.socket_path()` — the socket path currently in use.
// Signature is fixed by mlua's `create_function` contract.
#[allow(clippy::unnecessary_wraps)]
fn socket_path(_: &Lua, (): ()) -> LuaResult<String> {
    Ok(current_socket())
}

/// Builds the module table `require("paneru")` hands back.
///
/// # Errors
///
/// Returns an error if any Lua table/function creation or assignment fails.
pub fn module(lua: &Lua, version: &str) -> LuaResult<LuaTable> {
    let exports = lua.create_table()?;

    // Installs paneru.run/command and the typed paneru.window/workspace/mouse
    // tables on top of the socket dispatcher.
    crate::install(lua, &exports, &(Rc::new(dispatch) as crate::Dispatch))?;

    exports.set("query", lua.create_function(query)?)?;
    exports.set("query_json", lua.create_function(query_json)?)?;
    // The fixed-kind shorthands share one (name, kind) list with the embedded
    // runtime, so both hosts spell them the same and neither hardcodes a token.
    for (name, kind) in StateQueryKind::SHORTHANDS {
        exports.set(
            name,
            lua.create_function(move |lua, ()| query_json(lua, Some(kind.token().to_string())))?,
        )?;
    }

    exports.set("state", state_table(lua)?)?;

    exports.set("subscribe", lua.create_function(subscribe)?)?;
    exports.set("set_socket_path", lua.create_function(set_socket_path)?)?;
    exports.set("socket_path", lua.create_function(socket_path)?)?;

    exports.set("_VERSION", version)?;

    Ok(exports)
}

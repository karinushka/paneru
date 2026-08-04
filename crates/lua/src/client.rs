//! The client half of the API: talking to a running daemon over its Mach
//! service.
//!
//! [`module`] builds the complete table that `require("paneru")` returns — the
//! shared typed API from [`crate::install`], dispatching to the daemon, plus the
//! client-only `query_*` / `subscribe` / service-name helpers. The loadable
//! `module` feature's `luaopen_paneru` is then a one-liner returning this table.
//!
//! The service name defaults to `com.karinushka.paneru`, can be overridden with
//! the `PANERU_MACH_SERVICE` environment variable, or set at runtime via
//! `paneru.set_service_name("com.example.paneru")`.
//!
//! Every call here is a blocking round trip, because Lua's C API is synchronous
//! and a callback cannot yield into an executor. [`call`] is the one place that
//! drives a future, so the blocking happens once per request rather than being
//! scattered through every function.

use std::rc::Rc;
use std::sync::{LazyLock, Mutex};

use mlua::prelude::*;
use paneru_mach_ipc::Sender;
use paneru_shared_types::commands::Command;
use paneru_shared_types::script_state::ScriptStateWrite;
use paneru_shared_types::script_value::ScriptValue;
use paneru_shared_types::state::{StateEvent, StateQueryKind};
use paneru_shared_types::windowset_lua::returned_ops;
use paneru_shared_types::wire::{
    Request, Response, ScriptStateRequest, ScriptStateResponse, WriteOutcome,
};

/// The active service name, seeded from the shared default (and its environment
/// override) and mutable via `set_service_name`.
static SERVICE: LazyLock<Mutex<String>> =
    LazyLock::new(|| Mutex::new(paneru_shared_types::wire::service_name()));

fn service_name() -> String {
    SERVICE.lock().map_or_else(
        |_| paneru_shared_types::wire::SERVICE_NAME.to_string(),
        |guard| guard.clone(),
    )
}

/// Connects to the running daemon.
fn connect() -> LuaResult<Sender<Request>> {
    Sender::connect(&service_name()).map_err(|err| match err {
        paneru_mach_ipc::Error::NotRunning => {
            LuaError::RuntimeError("paneru is not running".to_string())
        }
        other => LuaError::external(other),
    })
}

/// Sends a request and waits for the answer.
///
/// The single place this module blocks. A daemon that reports a failure raises
/// it as a Lua error rather than returning it, so a script sees a failed call as
/// a failure instead of as a silent no-op.
fn call(request: &Request) -> LuaResult<Response> {
    let sender = connect()?;
    let response: Response = futures_lite::future::block_on(sender.call(request))
        .map_err(LuaError::external)?;

    match response {
        Response::Error(message) => Err(LuaError::RuntimeError(message)),
        other => Ok(other),
    }
}

/// Sends a request that expects no answer.
fn send(request: &Request) -> LuaResult<()> {
    let sender = connect()?;
    futures_lite::future::block_on(sender.send(request)).map_err(LuaError::external)
}

/// The primitive the shared API is built on: send the command to the daemon
/// (fire-and-forget).
// Takes `Command` by value to match the shared `crate::Dispatch` closure type,
// which every verb closure in `lib.rs` also has to satisfy.
fn dispatch(_: &Lua, command: Command) -> LuaResult<bool> {
    send(&Request::Command(command))?;
    Ok(true)
}

/// Runs a state query and returns the payload.
fn query_payload(kind: StateQueryKind) -> LuaResult<paneru_shared_types::wire::QueryPayload> {
    match call(&Request::Query(kind))? {
        Response::Query(payload) => Ok(payload),
        other => Err(unexpected(&other)),
    }
}

/// Reads the `kind` argument shared by `query` and `query_json`.
fn read_kind(kind: Option<String>) -> LuaResult<StateQueryKind> {
    let token = kind.unwrap_or_else(|| "state".to_string());
    StateQueryKind::parse(&token).ok_or_else(|| {
        LuaError::RuntimeError(format!(
            "unknown query '{token}', expected one of {}",
            StateQueryKind::tokens()
        ))
    })
}

/// `paneru.query(kind)` — run a state query and return the raw JSON string.
///
/// Kept for scripts that want to hand the text to something else; `query_json`
/// is what a script reading the answer itself wants.
fn query(_: &Lua, kind: Option<String>) -> LuaResult<String> {
    Ok(query_payload(read_kind(kind)?)?
        .to_json()
        .map_err(LuaError::external)?
        .to_string())
}

/// `paneru.query_json(kind)` — like `query` but decoded into a Lua value.
fn query_json(lua: &Lua, kind: Option<String>) -> LuaResult<LuaValue> {
    let json = query_payload(read_kind(kind)?)?
        .to_json()
        .map_err(LuaError::external)?;
    lua.to_value(&json)
}

/// Runs one script-state request.
fn script_state(request: ScriptStateRequest) -> LuaResult<ScriptStateResponse> {
    match call(&Request::ScriptState(request))? {
        Response::ScriptState(answer) => Ok(answer),
        other => Err(unexpected(&other)),
    }
}

/// Reads the current value of `key`.
///
/// A stored `Null` and an absent key both read as `nil` in Lua, which is the
/// only thing Lua can express — it has no way to hold "present but empty".
fn state_get(key: &str) -> LuaResult<Option<ScriptValue>> {
    match script_state(ScriptStateRequest::Get {
        key: key.to_string(),
    })? {
        ScriptStateResponse::Value(value) => Ok(value.filter(|value| !value.is_null())),
        ScriptStateResponse::Write(_) => Err(LuaError::RuntimeError(
            "paneru.state.get: the daemon answered a read with a write outcome".to_string(),
        )),
    }
}

/// Runs a write and returns its outcome.
fn state_write(write: ScriptStateWrite) -> LuaResult<WriteOutcome> {
    match script_state(ScriptStateRequest::Write(write))? {
        ScriptStateResponse::Write(outcome) => Ok(outcome),
        ScriptStateResponse::Value(_) => Err(LuaError::RuntimeError(
            "paneru.state: the daemon answered a write with a value".to_string(),
        )),
    }
}

/// Builds the `paneru.state` table: the same store, under the same names, as
/// the embedded runtime gives a script.
///
/// The one difference worth knowing is cost. Each call here is a round trip to
/// the daemon, where the embedded runtime reads a copy it already has; the
/// values are the same either way, and so is `mutate`'s guarantee — the daemon
/// is the one deciding whether a write still matches what it was read as, so a
/// client and a script contending on a key resolve against the same store.
fn state_table(lua: &Lua) -> LuaResult<LuaTable> {
    let state = lua.create_table()?;

    state.set(
        "get",
        lua.create_function(|lua, key: String| match state_get(&key)? {
            Some(value) => value.into_lua(lua),
            None => Ok(LuaValue::Nil),
        })?,
    )?;

    // `set(key, nil)` removes, matching the embedded spelling.
    state.set(
        "set",
        lua.create_function(|lua, (key, value): (String, LuaValue)| {
            let write = if value.is_nil() {
                ScriptStateWrite::remove(key)
            } else {
                ScriptStateWrite::set(key, ScriptValue::from_lua(value, lua)?)
            };
            state_write(write)?;
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
                let old = match current.clone() {
                    Some(value) => value.into_lua(lua)?,
                    None => LuaValue::Nil,
                };
                let returned: LuaValue = transform.call(old)?;
                let next = if returned.is_nil() {
                    None
                } else {
                    Some(ScriptValue::from_lua(returned, lua)?)
                };

                let outcome = state_write(ScriptStateWrite::compare_and_set(
                    key.clone(),
                    current.clone(),
                    next.clone(),
                ))?;

                match outcome {
                    WriteOutcome::Applied { .. } => {
                        return match next {
                            Some(value) => value.into_lua(lua),
                            None => Ok(LuaValue::Nil),
                        };
                    }
                    // Somebody else wrote the key in between; re-run the
                    // transform against what is actually there now.
                    WriteOutcome::Conflict { current: found } => current = found,
                }
            }
            Err(LuaError::RuntimeError(format!(
                "paneru.state.mutate: '{key}' kept changing under it after {ATTEMPTS} attempts"
            )))
        })?,
    )?;

    Ok(state)
}

/// `paneru.windows(fn)` — xmonad's `windows`: hand the window set to `fn` and
/// commit whatever it hands back.
///
/// The same contract as the embedded runtime's, and the same value: the daemon
/// serves this from `extract_window_set`, the very tree a `paneru.on` handler is
/// given. The transform is pure Lua either way, so a function written for one
/// host runs unchanged on the other.
///
/// Two round trips rather than the embedded runtime's shared per-batch read —
/// one to fetch, one to commit — and blocking rather than async, like every
/// other call here. A transform that returns nothing skips the second.
// Signature is fixed by mlua's `create_function` contract.
fn windows(lua: &Lua, transform: LuaFunction) -> LuaResult<bool> {
    let set = match call(&Request::WindowSet)? {
        Response::WindowSet(set) => *set,
        other => return Err(unexpected(&other)),
    };

    let returned: LuaValue = transform.call(lua.create_userdata(set)?)?;
    let ops = returned_ops(&returned)?;
    if ops.is_empty() {
        return Ok(false);
    }

    send(&Request::WindowSetApply(ops))?;
    Ok(true)
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

/// The name a filter matches an event against — the same `event` field the JSON
/// carries, so a filter written against the documented output still works.
fn event_name(event: &StateEvent) -> Option<String> {
    event
        .to_json()
        .ok()?
        .get("event")?
        .as_str()
        .map(str::to_string)
}

/// `paneru.subscribe(event, callback[, opts])` — stream events matching `event`
/// to `callback`. Blocks until the daemon exits, so run it in a dedicated
/// process. `event` is the event name to filter on (e.g. `"window_focused"`), a
/// table of several names, or `nil` for every event. Each event is a decoded Lua
/// table unless `opts.decode == false` (then the raw JSON line string).
fn subscribe(
    lua: &Lua,
    (event, callback, opts): (LuaValue, LuaFunction, Option<LuaTable>),
) -> LuaResult<bool> {
    use futures_lite::StreamExt;

    let events = read_events(&event)?;
    let decode = opts
        .as_ref()
        .and_then(|opts| opts.get::<Option<bool>>("decode").ok().flatten())
        .unwrap_or(true);

    let sender = connect()?;
    let stream = futures_lite::future::block_on(
        sender.subscribe::<StateEvent>(&Request::Subscribe),
    )
    .map_err(LuaError::external)?;
    let mut stream = std::pin::pin!(stream);

    loop {
        let delivery = futures_lite::future::block_on(stream.next());
        let Some(delivery) = delivery else { break };

        let event = match delivery {
            Ok(delivery) => delivery.value,
            // The daemon is gone; the subscription is over, which is how this
            // ends when the window manager stops.
            Err(paneru_mach_ipc::Error::PeerGone) => break,
            Err(err) => return Err(LuaError::external(err)),
        };

        if let Some(wanted) = &events {
            let name = event_name(&event);
            if !name.is_some_and(|name| wanted.contains(&name)) {
                continue;
            }
        }

        let json = event.to_json().map_err(LuaError::external)?;
        if decode {
            callback.call::<()>(lua.to_value(&json)?)?;
        } else {
            callback.call::<()>(json.to_string())?;
        }
    }
    Ok(true)
}

/// `paneru.set_service_name(name)` — override the daemon's Mach service name.
// Signature is fixed by mlua's `create_function` contract.
#[allow(clippy::unnecessary_wraps)]
fn set_service_name(_: &Lua, name: String) -> LuaResult<()> {
    if let Ok(mut guard) = SERVICE.lock() {
        *guard = name;
    }
    Ok(())
}

/// `paneru.service_name()` — the service name currently in use.
// Signature is fixed by mlua's `create_function` contract.
#[allow(clippy::unnecessary_wraps)]
fn service_name_fn(_: &Lua, (): ()) -> LuaResult<String> {
    Ok(service_name())
}

/// The daemon answered something this request never asks for, which means the
/// two ends disagree about the protocol.
fn unexpected(response: &Response) -> LuaError {
    LuaError::RuntimeError(format!("unexpected response from paneru: {response:?}"))
}

/// Builds the module table `require("paneru")` hands back.
///
/// # Errors
///
/// Returns an error if any Lua table/function creation or assignment fails.
pub fn module(lua: &Lua, version: &str) -> LuaResult<LuaTable> {
    let exports = lua.create_table()?;

    // Installs paneru.run/command and the typed paneru.window/workspace/mouse
    // tables on top of the daemon dispatcher.
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

    exports.set("windows", lua.create_function(windows)?)?;

    exports.set("subscribe", lua.create_function(subscribe)?)?;
    exports.set("set_service_name", lua.create_function(set_service_name)?)?;
    exports.set("service_name", lua.create_function(service_name_fn)?)?;

    exports.set("_VERSION", version)?;

    Ok(exports)
}

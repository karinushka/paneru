//! The `paneru` Lua API, in both the shapes it is used in.
//!
//! [`install`] puts the typed, host-agnostic API onto a table — `paneru.run`,
//! `paneru.window.*`, `paneru.workspace.*`, `paneru.mouse.*` — built on a
//! caller-supplied dispatcher. The daemon's embedded runtime (`src/lua`) hands
//! it one that queues the [`Command`] onto the command bus; [`client`] hands it
//! one that writes the command to a running daemon's Unix socket, and adds the
//! client-only `query_*` / `subscribe` helpers on top.
//!
//! With the `module` feature the crate additionally builds as a loadable Lua C
//! extension (`paneru.so`) exposing that client table:
//!
//! ```lua
//! local paneru = require("paneru")
//!
//! paneru.window.focus({ direction = "east" })   -- directional
//! paneru.window.focus({ number = 3 })           -- by column number
//! paneru.window.balance()
//! paneru.workspace.select({ number = 2 })       -- switch virtual workspace
//! paneru.workspace.move_window({ number = 2, follow = false })
//! paneru.workspace.add()                        -- create + switch to a new virtual workspace
//! paneru.quit()
//!
//! for _, window in ipairs(paneru.query_on_screen()) do  -- actually visible
//!   print(window.app_name, window.title)
//! end
//!
//! paneru.state.set("pads.term", 4213)          -- outlives reloads and restarts
//! paneru.state.mutate("count", function(n) return (n or 0) + 1 end)
//!
//! paneru.subscribe("window_focused", function(evt)  -- blocking; run in a helper process
//!   print(evt.title)
//! end)
//!
//! paneru.subscribe(nil, function(evt) ... end)  -- nil event = every event
//! paneru.subscribe({ "window_focused", "window_title_changed" }, function(evt) ... end)
//! ```
//!
//! Every verb builds a real [`Command`]: option tables are deserialized into
//! the actual enums with mlua's serde support, so `"east"` becomes
//! [`Direction::East`] and an unknown value fails at the call site. Only the
//! host-specific extras differ (`paneru.on` / `paneru.bind` are embedded-only;
//! `subscribe` and the socket-path helpers are client-only).

#![allow(
    clippy::needless_pass_by_value,
    reason = "mlua callback signatures are by-value by contract"
)]

pub mod client;

use std::rc::Rc;

use mlua::{Function, Lua, LuaSerdeExt, Result, Table, Value};
use regex::Regex;
use serde::Deserialize;

use paneru_shared_types::commands::{
    Command, Direction, MouseMove, MoveFocus, Operation, ResizeDirection, parse_command,
};

/// Issues a [`Command`]. The only thing the two hosts differ by.
pub type Dispatch = Rc<dyn Fn(&Lua, Command) -> Result<bool>>;

/// Installs the shared API onto the `paneru` table, building every verb on
/// `dispatch`.
///
/// # Errors
///
/// Returns an error if any Lua table/function creation or assignment fails.
pub fn install(lua: &Lua, paneru: &Table, dispatch: &Dispatch) -> Result<()> {
    // paneru.run(cmd) / paneru.command(cmd) — the escape hatch: a command
    // string, an argv table, or a structured command table.
    let run = {
        let dispatch = Rc::clone(dispatch);
        lua.create_function(move |lua, command: Value| dispatch(lua, to_command(lua, &command)?))?
    };
    paneru.set("run", run.clone())?;
    paneru.set("command", run)?;

    paneru.set("window", window_table(lua, dispatch)?)?;
    paneru.set("workspace", workspace_table(lua, dispatch)?)?;

    let mouse = lua.create_table()?;
    mouse.set(
        "next_display",
        verb(lua, dispatch, Command::Mouse(MouseMove::ToNextDisplay))?,
    )?;
    paneru.set("mouse", mouse)?;

    paneru.set("quit", verb(lua, dispatch, Command::Quit)?)?;
    paneru.set("restart", verb(lua, dispatch, Command::Restart)?)?;
    paneru.set("print_state", verb(lua, dispatch, Command::PrintState)?)?;

    // paneru.match{ app = …, bundle = …, title = …, floating = …, managed = … }
    // builds a predicate over window records, for `ws:find`/`ws:filter`.
    // `app`, `bundle` and `title` are regexes, compiled here so a bad pattern
    // errors at the call site rather than silently matching nothing.
    paneru.set("match", lua.create_function(matcher)?)?;

    Ok(())
}

/// One `paneru.match{…}` call: compiles the spec into a Lua predicate.
///
/// # Errors
///
/// Returns a Lua runtime error if regex compilation fails or an unknown field is present.
pub fn matcher(lua: &Lua, spec: Table) -> Result<Function> {
    let pattern = |field: &str| -> Result<Option<Regex>> {
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

    lua.create_function(move |_, window: Table| {
        let matches = |regex: &Option<Regex>, fields: &[&str]| -> Result<bool> {
            let Some(regex) = regex else {
                return Ok(true);
            };
            for field in fields {
                if let Ok(val) = window.get::<String>(*field) {
                    return Ok(regex.is_match(&val));
                }
            }
            Ok(false)
        };
        let flag = |want: Option<bool>, field: &str| -> Result<bool> {
            match want {
                Some(want) => Ok(window.get::<bool>(field)? == want),
                None => Ok(true),
            }
        };
        Ok(matches(&app, &["app_name", "app"])?
            && matches(&bundle, &["bundle_id", "bundle"])?
            && matches(&title, &["title"])?
            && flag(floating, "floating")?
            && flag(managed, "managed")?)
    })
}

/// Builds the `paneru.window` sub-table.
fn window_table(lua: &Lua, dispatch: &Dispatch) -> Result<Table> {
    let window = lua.create_table()?;

    window.set(
        "focus",
        directional(lua, dispatch, "window.focus", Operation::Focus)?,
    )?;
    window.set(
        "swap",
        directional(lua, dispatch, "window.swap", Operation::Swap)?,
    )?;
    window.set("resize", resize(lua, dispatch)?)?;
    window.set(
        "next_display",
        follower(lua, dispatch, Operation::ToNextDisplay)?,
    )?;

    for (name, operation) in [
        ("focus_managed", Operation::FocusManaged),
        ("focus_unmanaged", Operation::FocusUnmanaged),
        ("center", Operation::Center),
        ("snap", Operation::Snap),
        ("manage", Operation::Manage),
        ("equalize", Operation::Equalize),
        ("balance", Operation::Balance),
        ("stack", Operation::Stack(true)),
        ("unstack", Operation::Stack(false)),
        ("full_width", Operation::FullWidth),
        ("grow", Operation::Resize(ResizeDirection::Grow)),
        ("shrink", Operation::Resize(ResizeDirection::Shrink)),
        ("raise_floating", Operation::RaiseFloating),
        ("toggle_float_layer", Operation::ToggleFloatingLayer),
    ] {
        window.set(name, verb(lua, dispatch, Command::Window(operation))?)?;
    }

    Ok(window)
}

/// Builds the `paneru.workspace` sub-table.
fn workspace_table(lua: &Lua, dispatch: &Dispatch) -> Result<Table> {
    let workspace = lua.create_table()?;

    let select = {
        let dispatch = Rc::clone(dispatch);
        lua.create_function(move |lua, opts: Value| {
            let operation = virtual_operation(
                Opts::read(lua, &opts)?.target("workspace.select")?,
                Operation::VirtualNumber,
                Operation::Virtual,
            )?;
            dispatch(lua, Command::Window(operation))
        })?
    };
    workspace.set("select", select)?;

    let move_window = {
        let dispatch = Rc::clone(dispatch);
        lua.create_function(move |lua, opts: Value| {
            let opts = Opts::read(lua, &opts)?;
            let follow = opts.follow();
            let operation = virtual_operation(
                opts.target("workspace.move_window")?,
                |index| Operation::VirtualMoveNumber(index, follow),
                |direction| Operation::VirtualMove(direction, follow),
            )?;
            dispatch(lua, Command::Window(operation))
        })?
    };
    workspace.set("move_window", move_window)?;

    workspace.set(
        "add",
        verb(lua, dispatch, Command::Window(Operation::VirtualAdd))?,
    )?;

    Ok(workspace)
}

/// Virtual-workspace verbs come in two shapes: a position selects a numbered
/// workspace, a direction cycles through them.
fn virtual_operation(
    direction: Direction,
    numbered: impl Fn(u32) -> Operation,
    directional: impl Fn(Direction) -> Operation,
) -> Result<Operation> {
    match direction {
        Direction::Nth(index) => u32::try_from(index)
            .map(&numbered)
            .map_err(|_| mlua::Error::RuntimeError("workspace number is too large".into())),
        direction => Ok(directional(direction)),
    }
}

/// A zero-argument verb issuing a fixed command.
fn verb(lua: &Lua, dispatch: &Dispatch, command: Command) -> Result<Function> {
    let dispatch = Rc::clone(dispatch);
    lua.create_function(move |lua, ()| dispatch(lua, command.clone()))
}

/// A verb taking `{ direction = "east" }` or `{ number = 3 }` (or the bare
/// value), e.g. `paneru.window.focus{ number = 3 }`.
fn directional(
    lua: &Lua,
    dispatch: &Dispatch,
    what: &'static str,
    operation: impl Fn(Direction) -> Operation + 'static,
) -> Result<Function> {
    let dispatch = Rc::clone(dispatch);
    lua.create_function(move |lua, opts: Value| {
        let direction = Opts::read(lua, &opts)?.target(what)?;
        dispatch(lua, Command::Window(operation(direction)))
    })
}

/// A verb taking `{ follow = false }`, defaulting to following the window.
fn follower(
    lua: &Lua,
    dispatch: &Dispatch,
    operation: impl Fn(MoveFocus) -> Operation + 'static,
) -> Result<Function> {
    let dispatch = Rc::clone(dispatch);
    lua.create_function(move |lua, opts: Value| {
        let follow = Opts::read(lua, &opts)?.follow();
        dispatch(lua, Command::Window(operation(follow)))
    })
}

/// `paneru.window.resize{ direction = "grow" }`, defaulting to growing.
fn resize(lua: &Lua, dispatch: &Dispatch) -> Result<Function> {
    let dispatch = Rc::clone(dispatch);
    lua.create_function(move |lua, opts: Value| {
        let direction = ResizeOpts::read(lua, &opts)?;
        dispatch(lua, Command::Window(Operation::Resize(direction)))
    })
}

/// The options every verb accepts: `{ direction = "east" }`, `{ number = 3 }`,
/// `{ follow = false }`, or the bare `"east"` / `3`.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Opts {
    direction: Option<Direction>,
    number: Option<Direction>,
    follow: Option<bool>,
}

impl Opts {
    /// Reads the options off a call argument. A bare scalar is the direction.
    fn read(lua: &Lua, value: &Value) -> Result<Self> {
        match value {
            Value::Nil => Ok(Self::default()),
            Value::Table(_) => lua.from_value(value.clone()),
            bare => Ok(Self {
                direction: Some(lua.from_value(bare.clone())?),
                ..Self::default()
            }),
        }
    }

    /// The target of a directional verb; `what` names it in the error.
    fn target(self, what: &str) -> Result<Direction> {
        self.direction.or(self.number).ok_or_else(|| {
            mlua::Error::RuntimeError(format!(
                "{what} expects {{ direction = ... }} or {{ number = ... }}"
            ))
        })
    }

    /// Whether the focus follows a moved window. Following is the default.
    fn follow(&self) -> MoveFocus {
        MoveFocus::follows(self.follow.unwrap_or(true))
    }
}

/// `paneru.window.resize` options: `{ direction = "grow" }` or a bare
/// `"shrink"`, defaulting to growing.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ResizeOpts {
    direction: Option<ResizeDirection>,
}

impl ResizeOpts {
    fn read(lua: &Lua, value: &Value) -> Result<ResizeDirection> {
        let direction = match value {
            Value::Nil => None,
            Value::Table(_) => lua.from_value::<Self>(value.clone())?.direction,
            bare => Some(lua.from_value(bare.clone())?),
        };
        Ok(direction.unwrap_or(ResizeDirection::Grow))
    }
}

/// Converts a `paneru.run` argument into a [`Command`]: a command string, an
/// argv table, or a structured table deserialized straight into the enums.
fn to_command(lua: &Lua, value: &Value) -> Result<Command> {
    let parse = |argv: &[String]| {
        let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
        parse_command(&borrowed).map_err(|err| mlua::Error::RuntimeError(err.to_string()))
    };

    match value {
        Value::String(command) => {
            let command = command.to_str()?;
            let argv: Vec<String> = command.split_whitespace().map(str::to_string).collect();
            if argv.is_empty() {
                return Err(mlua::Error::RuntimeError("empty command".into()));
            }
            parse(&argv)
        }
        // A sequence is argv (`{"window", "focus", "east"}`); any other table is
        // a structured command (`{ window = { focus = "east" } }`).
        Value::Table(table) if table.raw_len() > 0 => {
            let argv: Vec<String> = table
                .clone()
                .sequence_values::<Value>()
                .map(|entry| scalar_token(&entry?))
                .collect::<Result<_>>()?;
            parse(&argv)
        }
        Value::Table(_) => lua.from_value(value.clone()),
        other => Err(mlua::Error::RuntimeError(format!(
            "command must be a string, argv table or command table, got {}",
            other.type_name()
        ))),
    }
}

/// Stringifies an argv element (integers without a decimal point, so `3` becomes
/// `"3"` and not `"3.0"`).
fn scalar_token(value: &Value) -> Result<String> {
    match value {
        Value::String(string) => Ok(string.to_str()?.to_string()),
        Value::Integer(number) => Ok(number.to_string()),
        Value::Number(number) => Ok(format!("{number}")),
        other => Err(mlua::Error::RuntimeError(format!(
            "command arguments must be strings or numbers, got {}",
            other.type_name()
        ))),
    }
}

/// The loadable-module entry point: `require("paneru")` calls `luaopen_paneru`,
/// which returns the client table. Only compiled for the `module` feature —
/// the daemon depends on this crate as a plain library.
#[cfg(feature = "module")]
#[mlua::lua_module]
fn paneru(lua: &Lua) -> Result<Table> {
    client::module(lua, env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Installs the API with a dispatcher that records commands instead of
    /// issuing them, and runs `source`.
    fn run(source: &str) -> mlua::Result<Vec<Command>> {
        let lua = Lua::new();
        let issued = Rc::new(RefCell::new(Vec::new()));
        let paneru = lua.create_table()?;
        lua.globals().set("paneru", paneru.clone())?;

        let recorder = {
            let issued = Rc::clone(&issued);
            move |_: &Lua, command: Command| {
                issued.borrow_mut().push(command);
                Ok(true)
            }
        };
        install(&lua, &paneru, &(Rc::new(recorder) as Dispatch))?;
        lua.load(source).exec()?;

        let commands = issued.borrow().clone();
        Ok(commands)
    }

    fn debug(commands: &[Command]) -> Vec<String> {
        commands.iter().map(|c| format!("{c:?}")).collect()
    }

    #[test]
    fn typed_verbs_build_typed_commands() {
        let commands = run(r#"
            paneru.window.focus({ direction = "east" })
            paneru.window.focus({ number = 3 })
            paneru.window.balance()
            paneru.workspace.move_window({ number = 2, follow = false })
        "#)
        .unwrap();

        assert_eq!(
            debug(&commands),
            debug(&[
                Command::Window(Operation::Focus(Direction::East)),
                Command::Window(Operation::Focus(Direction::Nth(2))),
                Command::Window(Operation::Balance),
                Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
            ])
        );
    }

    #[test]
    fn run_accepts_strings_argv_and_command_tables() {
        let commands = run(r#"
            paneru.run("window focus east")
            paneru.run({ "window", "focus", 3 })
            paneru.run({ window = { focus = "east" } })
        "#)
        .unwrap();

        assert_eq!(
            debug(&commands),
            debug(&[
                Command::Window(Operation::Focus(Direction::East)),
                Command::Window(Operation::Focus(Direction::Nth(2))),
                Command::Window(Operation::Focus(Direction::East)),
            ])
        );
    }

    #[test]
    fn bad_arguments_fail_at_the_call_site() {
        assert!(run(r#"paneru.window.focus({ direction = "sideways" })"#).is_err());
        assert!(run("paneru.window.focus({})").is_err());
        assert!(run(r#"paneru.window.resize({ direction = "wider" })"#).is_err());
        assert!(run(r#"paneru.run("not a command")"#).is_err());
    }

    #[test]
    fn defaults_match_the_documented_behaviour() {
        let commands = run(r"
            paneru.window.resize()
            paneru.window.next_display()
            paneru.window.next_display({ follow = false })
        ")
        .unwrap();

        assert_eq!(
            debug(&commands),
            debug(&[
                Command::Window(Operation::Resize(ResizeDirection::Grow)),
                Command::Window(Operation::ToNextDisplay(MoveFocus::Follow)),
                Command::Window(Operation::ToNextDisplay(MoveFocus::Stay)),
            ])
        );
    }
}

//! The embedded Lua interpreter and everything a script registers into it.
//!
//! This module knows nothing about Bevy. It takes source in, hands dispatched
//! side effects back out as plain values ([`Command`]s and flash messages), and
//! reaches the world only through the `extract` callback passed to
//! [`LuaRuntime::with_query`]. That keeps the interpreter — which `mlua` makes
//! `!Send`, and which runs arbitrary user code of unbounded duration — free to
//! live on a thread of its own, with the ECS on the other side of a channel.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use mlua::{AnyUserData, Function, IntoLua, Lua, LuaSerdeExt, Table, Value};
use tracing::{error, warn};

use super::api;
use super::windowset::{LuaWindowSet, WINDOW_SET_PROVIDER};
use crate::commands::Command;
use crate::ecs::state::{PaneruQueryState, StateQueryKind};
use crate::platform::Modifiers;
use paneru_shared_types::windowset::WindowSet;

/// A Lua-registered keybind: `(keycode, modifiers, handler_id)`.
pub type LuaKeybind = (u8, Modifiers, u32);

/// Prepends `PANERU_LUA_PATH`/`PANERU_LUA_CPATH` (if set) onto `package.path`/
/// `package.cpath`, so `require("sbar")` (or any module supplied via the Nix
/// `extraLuaPackages` option) resolves. Each var is itself a `;`-separated
/// list of Lua path templates - the same shape `package.path` already uses -
/// so nixpkgs' `getLuaPath`/`getLuaCPath` output passes straight through with
/// no reformatting.
fn extend_lua_search_paths(lua: &Lua) -> mlua::Result<()> {
    let package: Table = lua.globals().get("package")?;
    prepend_env_path(&package, "path", "PANERU_LUA_PATH")?;
    prepend_env_path(&package, "cpath", "PANERU_LUA_CPATH")?;
    Ok(())
}

/// Prepends `$env_var`'s value onto `package[field]`, so caller-supplied
/// modules are found before Lua's compiled-in defaults.
fn prepend_env_path(package: &Table, field: &str, env_var: &str) -> mlua::Result<()> {
    let Ok(extra) = std::env::var(env_var) else {
        return Ok(());
    };
    let extra = extra.trim_matches(';');
    if extra.is_empty() {
        return Ok(());
    }
    let existing: String = package.get(field)?;
    package.set(field, format!("{extra};{existing}"))?;
    Ok(())
}

/// Everything the script registered, kept on the Rust side so dispatch never
/// has to reach back into Lua globals to find a callback.
#[derive(Default)]
pub(super) struct Registry {
    /// `paneru.on` handlers, in registration order per event name.
    pub(super) handlers: HashMap<String, Vec<Function>>,
    /// `paneru.bind` handlers indexed by `id - 1`: a Lua function, or a command
    /// string to run as-is.
    pub(super) binds: Vec<Value>,
    /// Chords parallel to `binds`, for publishing to the event-tap registry.
    pub(super) keybinds: Vec<LuaKeybind>,
}

/// Shared handle to the [`Registry`], captured by the registration closures and
/// read back when dispatching.
pub(super) type SharedRegistry = Rc<RefCell<Registry>>;

/// Pending side effects produced by Lua callbacks, drained after each dispatch.
#[derive(Default)]
pub(super) struct Outbox {
    /// Commands queued via `paneru.run` / `paneru.cmd`.
    pub(super) commands: Vec<Command>,
    /// Flash messages queued via `paneru.flash` as `(message, duration_secs)`.
    pub(super) flashes: Vec<(String, f32)>,
}

/// The side effects one dispatch produced: commands to put on the bus and
/// flash messages to show.
pub(super) type Effects = (Vec<Command>, Vec<(String, f32)>);

/// The embedded Lua runtime and its shared registration state.
pub struct LuaRuntime {
    lua: Lua,
    outbox: Rc<RefCell<Outbox>>,
    registry: SharedRegistry,
}

impl LuaRuntime {
    /// Builds a runtime by reading and executing the script at `path`.
    pub fn from_file(path: &Path) -> mlua::Result<Self> {
        let source = std::fs::read_to_string(path).map_err(mlua::Error::external)?;
        Self::from_source(&source)
    }

    /// Builds a runtime from Lua source, installing the `paneru` API and
    /// executing the script. Registered keybinds are collected for publishing.
    pub fn from_source(source: &str) -> mlua::Result<Self> {
        // SAFETY: the runtime is confined to the thread that built it, and
        // unsafe libs are what let a script `require` a C module such as
        // sketchybar's.
        let lua = unsafe { Lua::unsafe_new() };
        extend_lua_search_paths(&lua)?;
        let outbox = Rc::new(RefCell::new(Outbox::default()));
        let registry = SharedRegistry::default();
        api::install(&lua, &outbox, &registry)?;
        lua.load(source).exec()?;
        Ok(Self {
            lua,
            outbox,
            registry,
        })
    }

    pub(super) fn lua(&self) -> &Lua {
        &self.lua
    }

    /// The `(keycode, modifiers, id)` keybinds registered by the script, for
    /// publishing to the event-tap registry.
    pub fn published_keybinds(&self) -> Vec<LuaKeybind> {
        self.registry.borrow().keybinds.clone()
    }

    /// Whether the script registered any `paneru.on` handler. Building the Lua
    /// table for an event is the costly step, so a script that only binds keys
    /// pays nothing for events it can never observe.
    pub(super) fn has_event_handlers(&self) -> bool {
        !self.registry.borrow().handlers.is_empty()
    }

    /// Runs every `paneru.on(name, ...)` handler. Handlers are cloned out of the
    /// registry first so a handler that registers another one cannot deadlock on
    /// the borrow. One failing handler does not stop the rest.
    pub(super) fn dispatch_event(&self, name: &str, event: &Table) {
        let handlers = self
            .registry
            .borrow()
            .handlers
            .get(name)
            .cloned()
            .unwrap_or_default();
        for handler in handlers {
            let window_set = match self.lazy_window_set() {
                Ok(window_set) => window_set,
                Err(err) => {
                    error!("lua event handler '{name}': {err}");
                    continue;
                }
            };
            match handler.call::<Value>((event.clone(), window_set)) {
                Ok(returned) => self.commit(&returned, &format!("event handler '{name}'")),
                Err(err) => error!("lua event handler '{name}': {err}"),
            }
        }
    }

    /// Runs the handler bound to keybind `id`: a Lua function gets the window
    /// set, a command string goes straight onto the outbox.
    pub(super) fn dispatch_bind(&self, id: u32) {
        let handler = id
            .checked_sub(1)
            .and_then(|index| self.registry.borrow().binds.get(index as usize).cloned());
        match handler {
            Some(Value::Function(handler)) => {
                let window_set = match self.lazy_window_set() {
                    Ok(window_set) => window_set,
                    Err(err) => {
                        error!("lua keybind handler {id}: {err}");
                        return;
                    }
                };
                match handler.call::<Value>(window_set) {
                    Ok(returned) => self.commit(&returned, &format!("keybind handler {id}")),
                    Err(err) => error!("lua keybind handler {id}: {err}"),
                }
            }
            Some(Value::String(command)) => {
                let command = command.to_string_lossy();
                let argv: Vec<&str> = command.split_whitespace().collect();
                match crate::config::parse_command(&argv) {
                    Ok(command) => self.outbox.borrow_mut().commands.push(command),
                    Err(err) => error!("lua keybind {id} command '{command}': {err}"),
                }
            }
            _ => warn!("lua keybind {id} has no handler"),
        }
    }

    /// The window set a handler is handed: empty until it is first used, so a
    /// handler that ignores it costs nothing. See [`super::windowset`].
    fn lazy_window_set(&self) -> mlua::Result<AnyUserData> {
        self.lua.create_userdata(LuaWindowSet::lazy())
    }

    /// Queues whatever a handler returned.
    ///
    /// Returning a window set is how a handler asks for anything to change:
    /// the operations recorded on the value it hands back are what get replayed
    /// against the live world. A handler that returns nothing has asked for
    /// nothing, which is what makes computing a set and *not* returning it free
    /// of consequences.
    fn commit(&self, returned: &Value, context: &str) {
        match returned {
            Value::Nil => {}
            Value::UserData(data) => {
                if let Ok(window_set) = data.borrow::<LuaWindowSet>() {
                    let ops = window_set.ops();
                    if !ops.is_empty() {
                        self.outbox.borrow_mut().commands.push(Command::Layout(ops));
                    }
                } else {
                    warn!("lua {context} returned userdata that is not a window set");
                }
            }
            other => warn!(
                "lua {context} returned {}; a handler returns a window set, or nothing",
                other.type_name()
            ),
        }
    }

    /// Takes everything the callbacks queued, leaving the outbox empty.
    pub(super) fn drain_outbox(&self) -> Effects {
        let mut outbox = self.outbox.borrow_mut();
        (
            outbox.commands.drain(..).collect(),
            outbox.flashes.drain(..).collect(),
        )
    }

    /// Runs `body` with `paneru.query*` able to answer from the live world.
    ///
    /// The provider is a scoped Lua function borrowing `extract`, so it exists
    /// only while `body` runs: outside a callback there is no world access at
    /// all, and `paneru.query` says so rather than handing back a stale
    /// snapshot. `extract` is called lazily and at most once per dispatch — it
    /// reads every window's title over the accessibility API, so a script that
    /// never queries never pays for it, however hot the event.
    pub(super) fn with_query<R>(
        &self,
        extract: &dyn Fn() -> Result<PaneruQueryState, String>,
        extract_set: &dyn Fn() -> Result<WindowSet, String>,
        body: impl FnOnce() -> R,
    ) -> mlua::Result<R> {
        let cached: RefCell<Option<Rc<PaneruQueryState>>> = RefCell::new(None);
        let cached_set: RefCell<Option<WindowSet>> = RefCell::new(None);
        let result = self.lua.scope(|scope| {
            let provider = scope.create_function(|lua, (kind, as_json): (String, bool)| {
                let kind = StateQueryKind::parse(&kind).ok_or_else(|| {
                    mlua::Error::RuntimeError(format!("paneru.query: unknown kind '{kind}'"))
                })?;
                let state = {
                    let mut cached = cached.borrow_mut();
                    if cached.is_none() {
                        *cached = Some(Rc::new(extract().map_err(|err| {
                            mlua::Error::RuntimeError(format!("paneru.query: {err}"))
                        })?));
                    }
                    cached.clone().expect("just filled in")
                };
                if as_json {
                    state
                        .to_query_json(kind)
                        .map_err(mlua::Error::external)?
                        .into_lua(lua)
                } else {
                    let value = state.to_query_value(kind).map_err(mlua::Error::external)?;
                    lua.to_value(&value)
                }
            })?;
            self.lua
                .set_named_registry_value(api::QUERY_PROVIDER, provider)?;

            // The window set is fetched on the same terms: lazily, and at most
            // once however many handlers in this batch ask for one.
            let set_provider = scope.create_function(|lua, ()| {
                let set = {
                    let mut cached = cached_set.borrow_mut();
                    if cached.is_none() {
                        *cached = Some(extract_set().map_err(|err| {
                            mlua::Error::RuntimeError(format!("paneru window set: {err}"))
                        })?);
                    }
                    cached.clone().expect("just filled in")
                };
                lua.create_userdata(LuaWindowSet::materialised(set))
            })?;
            self.lua
                .set_named_registry_value(WINDOW_SET_PROVIDER, set_provider)?;

            Ok(body())
        });
        // Whatever happened, the scoped functions are dead once the scope ends.
        self.lua.unset_named_registry_value(api::QUERY_PROVIDER)?;
        self.lua.unset_named_registry_value(WINDOW_SET_PROVIDER)?;
        result
    }

    /// Runs `body` with `paneru.query*` wired to `extract`, logs any failure
    /// under `context`, then hands back the commands and flash messages its
    /// callbacks queued. The shared tail of both dispatch paths, which differ
    /// only in what they collect up front and what `body` does with it.
    pub(super) fn dispatch_with_query(
        &self,
        context: &str,
        extract: &dyn Fn() -> Result<PaneruQueryState, String>,
        extract_set: &dyn Fn() -> Result<WindowSet, String>,
        body: impl FnOnce(),
    ) -> Effects {
        if let Err(err) = self.with_query(extract, extract_set, body) {
            error!("lua {context}: {err}");
        }
        self.drain_outbox()
    }

    /// An empty runtime (no handlers, no binds). Used as a fallback when the
    /// init script fails to load, so the resource always exists and a later
    /// hot reload can install a fixed script.
    pub fn empty() -> Self {
        Self::from_source("").expect("empty Lua runtime should always build")
    }
}

#[cfg(test)]
mod tests {
    use super::super::convert;
    use super::*;
    use crate::events::Event;

    /// A window-set extractor for the tests that never ask for one. Calling it
    /// is a test failure, not a runtime error: it means the laziness broke.
    fn no_window_set() -> Result<paneru_shared_types::windowset::WindowSet, String> {
        panic!("this test should never materialise a window set");
    }

    /// Drains the outbox commands for assertions.
    fn drained_commands(runtime: &LuaRuntime) -> Vec<Command> {
        runtime.outbox.borrow_mut().commands.drain(..).collect()
    }

    #[test]
    fn bind_registers_keybind_and_stores_handler() {
        let runtime =
            LuaRuntime::from_source(r#"paneru.bind("alt - j", "window focus east")"#).unwrap();
        let binds = runtime.published_keybinds();
        assert_eq!(binds.len(), 1);
        let (_, modifiers, id) = binds[0];
        assert_eq!(modifiers, Modifiers::ALT);
        assert_eq!(id, 1);
    }

    #[test]
    fn string_keybind_dispatch_queues_command() {
        let runtime =
            LuaRuntime::from_source(r#"paneru.bind("alt - b", "window balance")"#).unwrap();
        runtime.dispatch_bind(1);
        let commands = drained_commands(&runtime);
        assert!(
            matches!(
                commands.as_slice(),
                [Command::Window(crate::commands::Operation::Balance)]
            ),
            "expected a balance command, got {commands:?}"
        );
    }

    #[test]
    fn function_keybind_can_run_commands() {
        let runtime = LuaRuntime::from_source(
            r#"paneru.bind("alt - j", function(state) paneru.run("window focus east") end)"#,
        )
        .unwrap();
        runtime.dispatch_bind(1);
        assert_eq!(drained_commands(&runtime).len(), 1);
    }

    #[test]
    fn event_handler_receives_event_and_queues_command() {
        let runtime = LuaRuntime::from_source(
            r#"paneru.on("space_changed", function(e) paneru.run("window balance") end)"#,
        )
        .unwrap();
        let (name, table) = convert::event_to_lua(runtime.lua(), &Event::SpaceChanged).unwrap();
        runtime.dispatch_event(&name, &table);
        assert_eq!(drained_commands(&runtime).len(), 1);
    }

    #[test]
    fn invalid_command_string_is_reported_not_panicking() {
        // A bad command string surfaces as a Lua runtime error at bind time.
        let result = LuaRuntime::from_source(r#"paneru.run("definitely not a command")"#);
        assert!(result.is_err());
    }

    /// A canned state document to answer queries with.
    fn test_state() -> PaneruQueryState {
        use crate::ecs::state::{
            PaneruActiveState, PaneruVirtualWorkspaceState, PaneruWindowState,
        };

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

    #[test]
    fn query_is_answered_from_the_provided_state() {
        let runtime = LuaRuntime::from_source(
            r#"
            paneru.bind("alt - q", function()
              local active = paneru.query_active()
              paneru.flash(active.focused_app_name)
              paneru.flash(tostring(#paneru.query_workspaces()))
              paneru.flash(paneru.query("active"))
            end)
            "#,
        )
        .unwrap();
        let extract = || Ok(test_state());
        runtime
            .with_query(&extract, &no_window_set, || runtime.dispatch_bind(1))
            .unwrap();

        let flashes: Vec<String> = runtime
            .outbox
            .borrow_mut()
            .flashes
            .drain(..)
            .map(|(message, _)| message)
            .collect();
        assert_eq!(flashes.len(), 3, "every query should have returned");
        assert_eq!(flashes[0], "Test App");
        assert_eq!(flashes[1], "1");
        assert!(
            flashes[2].starts_with('{') && flashes[2].contains("\"focused_app_name\":\"Test App\""),
            "paneru.query should return raw JSON, got {}",
            flashes[2]
        );
    }

    #[test]
    fn the_state_is_extracted_once_per_dispatch_and_only_on_demand() {
        let runtime = LuaRuntime::from_source(
            r#"
            paneru.bind("alt - q", function()
              paneru.query_active()
              paneru.query_on_screen()
            end)
            paneru.bind("alt - w", function() paneru.flash("no query here") end)
            "#,
        )
        .unwrap();

        let extractions = RefCell::new(0);
        let extract = || {
            *extractions.borrow_mut() += 1;
            Ok(test_state())
        };
        runtime
            .with_query(&extract, &no_window_set, || runtime.dispatch_bind(2))
            .unwrap();
        assert_eq!(
            *extractions.borrow(),
            0,
            "a handler that never queries pays nothing"
        );

        runtime
            .with_query(&extract, &no_window_set, || runtime.dispatch_bind(1))
            .unwrap();
        assert_eq!(*extractions.borrow(), 1, "two queries share one extraction");
    }

    #[test]
    fn query_outside_a_callback_explains_itself() {
        let Err(error) = LuaRuntime::from_source("paneru.query_state()") else {
            panic!("there is no world to query at script top level");
        };
        let error = error.to_string();
        assert!(
            error.contains("only available inside"),
            "expected an explanation, got {error}"
        );

        // ...and the provider does not outlive the dispatch that installed it.
        let runtime = LuaRuntime::from_source(
            r#"
            escaped = nil
            paneru.bind("alt - q", function() escaped = paneru.query_state end)
            "#,
        )
        .unwrap();
        let extract = || Ok(test_state());
        runtime
            .with_query(&extract, &no_window_set, || runtime.dispatch_bind(1))
            .unwrap();
        assert!(
            runtime.lua().load("escaped()").exec().is_err(),
            "a query captured during dispatch should not answer afterwards"
        );
    }

    #[test]
    fn a_window_set_outside_a_callback_explains_itself() {
        // Same contract as `paneru.query`: there is no world to read at script
        // top level, so say so rather than answering from nothing.
        let runtime = LuaRuntime::from_source("").unwrap();
        let error = runtime
            .lua()
            // The set has to actually be *used*: it is lazy, so merely
            // being handed one costs nothing and cannot fail.
            .load("return paneru.windows(function(ws) return ws:focus(1) end)")
            .exec()
            .expect_err("there is no window set at script top level")
            .to_string();
        assert!(
            error.contains("only available inside"),
            "expected an explanation, got {error}"
        );
    }

    #[test]
    fn unknown_query_kinds_are_rejected() {
        let runtime = LuaRuntime::from_source("").unwrap();
        let extract = || Ok(test_state());
        let error = runtime
            .with_query(&extract, &no_window_set, || {
                runtime
                    .lua()
                    .load(r#"return paneru.query("windows")"#)
                    .exec()
            })
            .unwrap()
            .expect_err("'windows' is not a query kind")
            .to_string();
        assert!(
            error.contains("unknown kind 'windows'") && error.contains("on-screen"),
            "the error should list the valid kinds, got {error}"
        );
    }

    #[test]
    fn dispatch_hands_back_what_the_callbacks_queued() {
        let runtime = LuaRuntime::from_source(
            r#"
            paneru.bind("alt - b", function()
              paneru.run("window balance")
              paneru.flash("done", 3.0)
            end)
            "#,
        )
        .unwrap();
        let extract = || Ok(test_state());
        let (commands, flashes) =
            runtime.dispatch_with_query("test", &extract, &no_window_set, || {
                runtime.dispatch_bind(1);
            });
        assert_eq!(commands.len(), 1);
        assert_eq!(flashes, vec![("done".to_string(), 3.0)]);
        // ...and the outbox is empty afterwards, so nothing is delivered twice.
        assert!(runtime.drain_outbox().0.is_empty());
    }

    #[test]
    fn empty_runtime_has_no_binds() {
        let runtime = LuaRuntime::empty();
        assert!(runtime.published_keybinds().is_empty());
    }

    #[test]
    fn extra_lua_path_env_var_is_prepended() {
        // Unique var name so this doesn't race other tests' env state.
        // SAFETY: no other test reads or writes this variable.
        unsafe { std::env::set_var("PANERU_LUA_PATH", "/tmp/paneru-test/?.lua") };
        let runtime = LuaRuntime::from_source("").unwrap();
        let package: Table = runtime.lua().globals().get("package").unwrap();
        let path: String = package.get("path").unwrap();
        // SAFETY: no other test reads or writes this variable.
        unsafe { std::env::remove_var("PANERU_LUA_PATH") };
        assert!(
            path.starts_with("/tmp/paneru-test/?.lua;"),
            "expected the extra path to be prepended, got {path}"
        );
    }
}

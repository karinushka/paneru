//! The `WindowSet` as a script sees it.
//!
//! A thin `UserData` shell around [`paneru_shared_types::windowset::WindowSet`].
//! The value itself is pure and lives in the shared crate; everything here is
//! marshalling — plus one piece of behaviour worth knowing about.
//!
//! # Laziness
//!
//! Handlers are given a window set whether they want one or not, and building
//! it reads every window's title over the accessibility API and costs a
//! round-trip to the main thread. So the value a handler receives starts empty
//! and fetches itself the first time a method is called on it. A handler that
//! ignores its argument — every `paneru.bind("…", "window focus east")`, and
//! any `paneru.on` that only looks at the event — pays nothing, however hot the
//! event.
//!
//! The fetch goes through a provider installed in the Lua registry for exactly
//! as long as a callback is on the stack, so a set that was never used inside
//! one — stashed in a global at script top level, say — says so rather than
//! answering from nothing. A set that *was* used keeps the tree it fetched: it
//! is a value, not a view, so a handler that holds one sees what it saw.
//!
//! # Indices
//!
//! Lua counts from one. Column indices and workspace numbers here do too, and
//! are translated at this boundary rather than anywhere deeper.

use std::cell::RefCell;

use mlua::{Function, Lua, LuaSerdeExt, UserData, UserDataMethods, Value};
use paneru_shared_types::windowset::{LayoutOp, RelativeRect, WinID, WindowSet};

/// Reads `{ x = …, y = …, width = …, height = … }` as fractions of a display.
/// Missing fields default to a full-display rect, so `{ width = 0.5 }` is the
/// left half.
fn relative_rect(rect: &mlua::Table) -> mlua::Result<RelativeRect> {
    let field = |name: &str, default: f64| -> mlua::Result<f64> {
        Ok(rect.get::<Option<f64>>(name)?.unwrap_or(default))
    };
    Ok(RelativeRect {
        x: field("x", 0.0)?,
        y: field("y", 0.0)?,
        width: field("width", 1.0)?,
        height: field("height", 1.0)?,
    })
}

/// A window set on the Lua side: either already fetched, or waiting to be.
pub(super) struct LuaWindowSet {
    set: RefCell<Option<WindowSet>>,
}

impl LuaWindowSet {
    /// A window set that already holds its tree.
    ///
    /// There is no lazy form any more. Fetching on first use meant a
    /// synchronous provider call from inside these methods, which cannot
    /// suspend — and it saved nothing once every handler in a batch came to
    /// share one read: see [`super::world::DispatchWorld`].
    pub(super) fn materialised(set: WindowSet) -> Self {
        Self {
            set: RefCell::new(Some(set)),
        }
    }

    /// What the handler asked for, if it transformed the set at all.
    pub(super) fn ops(&self) -> Vec<LayoutOp> {
        self.set
            .borrow()
            .as_ref()
            .map(WindowSet::ops)
            .unwrap_or_default()
    }

    /// The tree this set was built around.
    fn resolve(&self, _lua: &Lua) -> mlua::Result<WindowSet> {
        self.set
            .borrow()
            .clone()
            .ok_or_else(|| mlua::Error::RuntimeError("the window set is empty".into()))
    }
}

impl UserData for LuaWindowSet {
    #[allow(clippy::too_many_lines)]
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // --- reading ---------------------------------------------------

        methods.add_method("focused", |lua, this, ()| Ok(this.resolve(lua)?.focused()));

        methods.add_method("windows", |lua, this, ()| {
            let set = this.resolve(lua)?;
            set.windows()
                .map(|window| lua.to_value(window))
                .collect::<mlua::Result<Vec<Value>>>()
        });

        methods.add_method("window", |lua, this, id: WinID| {
            let set = this.resolve(lua)?;
            match set.window(id) {
                Some(window) => lua.to_value(window),
                None => Ok(Value::Nil),
            }
        });

        // The active workspace of the active display — "here", for a script.
        methods.add_method("current", |lua, this, ()| {
            Ok(this
                .resolve(lua)?
                .current()
                .map(|workspace| workspace.number))
        });

        methods.add_method("workspaces", |lua, this, ()| {
            Ok(this
                .resolve(lua)?
                .workspaces()
                .map(|workspace| workspace.number)
                .collect::<Vec<u32>>())
        });

        // Every window on a workspace, tiled first then floating.
        methods.add_method("workspace_windows", |lua, this, number: u32| {
            let set = this.resolve(lua)?;
            let Some(workspace) = set.workspace(number) else {
                return Ok(Vec::new());
            };
            workspace
                .windows()
                .map(|window| lua.to_value(window))
                .collect::<mlua::Result<Vec<Value>>>()
        });

        // The workspace's columns, each a list of window ids, left to right.
        methods.add_method("columns", |lua, this, number: Option<u32>| {
            let set = this.resolve(lua)?;
            let workspace = match number {
                Some(number) => set.workspace(number),
                None => set.current(),
            };
            let Some(workspace) = workspace else {
                return Ok(Vec::new());
            };
            Ok(workspace
                .columns
                .iter()
                .map(|column| column.windows.iter().map(|window| window.id).collect())
                .collect::<Vec<Vec<WinID>>>())
        });

        // One-based, like every other index a Lua script handles.
        methods.add_method("column_of", |lua, this, id: WinID| {
            Ok(this.resolve(lua)?.column_of(id).map(|index| index + 1))
        });

        methods.add_method("workspace_of", |lua, this, id: WinID| {
            Ok(this
                .resolve(lua)?
                .workspace_of(id)
                .map(|workspace| workspace.number))
        });

        // The whole display record, so a script can work out pixel geometry
        // itself when fractions are not enough.
        methods.add_method("display_of", |lua, this, id: WinID| {
            let set = this.resolve(lua)?;
            let Some(display) = set.display_of(id) else {
                return Ok(Value::Nil);
            };
            let table = lua.create_table()?;
            table.set("id", display.id)?;
            table.set("active", display.active)?;
            table.set("x", display.frame.x)?;
            table.set("y", display.frame.y)?;
            table.set("width", display.frame.width)?;
            table.set("height", display.frame.height)?;
            Ok(Value::Table(table))
        });

        methods.add_method("east", |lua, this, id: WinID| {
            Ok(this.resolve(lua)?.east(id))
        });
        methods.add_method("west", |lua, this, id: WinID| {
            Ok(this.resolve(lua)?.west(id))
        });
        methods.add_method("next", |lua, this, id: WinID| {
            Ok(this.resolve(lua)?.next(id))
        });
        methods.add_method("prev", |lua, this, id: WinID| {
            Ok(this.resolve(lua)?.prev(id))
        });

        // Predicates are plain Lua functions taking a window record, so
        // `paneru.match` is a convenience rather than a requirement — an
        // inline `function(w) return w.title:match("scratch") end` works too.
        methods.add_method("find", |lua, this, predicate: Function| {
            let set = this.resolve(lua)?;
            for window in set.windows() {
                let record = lua.to_value(window)?;
                if predicate.call::<bool>(record.clone())? {
                    return Ok(record);
                }
            }
            Ok(Value::Nil)
        });

        methods.add_method("filter", |lua, this, predicate: Function| {
            let set = this.resolve(lua)?;
            let mut matched = Vec::new();
            for window in set.windows() {
                let record = lua.to_value(window)?;
                if predicate.call::<bool>(record.clone())? {
                    matched.push(record);
                }
            }
            Ok(matched)
        });

        // --- transforming ----------------------------------------------
        //
        // Each returns a *new* window set. The one it was called on is
        // untouched, and nothing reaches a real window until a handler returns
        // a set carrying these operations.

        methods.add_method("focus", |lua, this, id: WinID| {
            Ok(LuaWindowSet::materialised(this.resolve(lua)?.focus(id)))
        });

        methods.add_method("swap", |lua, this, (first, second): (WinID, WinID)| {
            Ok(LuaWindowSet::materialised(
                this.resolve(lua)?.swap(first, second),
            ))
        });

        methods.add_method(
            "shift",
            |lua, this, (id, workspace, follow): (WinID, u32, Option<bool>)| {
                Ok(LuaWindowSet::materialised(
                    this.resolve(lua)?
                        .shift_following(id, workspace, follow.unwrap_or(false)),
                ))
            },
        );

        methods.add_method("view", |lua, this, workspace: u32| {
            Ok(LuaWindowSet::materialised(
                this.resolve(lua)?.view(workspace),
            ))
        });

        // `ws:float(id)` leaves the window where it is (defaultFloating);
        // `ws:float(id, rect)` places it (customFloating). The rect is given as
        // fractions of the display, like xmonad's RationalRect.
        methods.add_method(
            "float",
            |lua, this, (id, rect): (WinID, Option<mlua::Table>)| {
                let set = this.resolve(lua)?;
                let Some(rect) = rect else {
                    return Ok(LuaWindowSet::materialised(set.float(id)));
                };
                Ok(LuaWindowSet::materialised(
                    set.float_at(id, relative_rect(&rect)?),
                ))
            },
        );

        methods.add_method("sink", |lua, this, id: WinID| {
            Ok(LuaWindowSet::materialised(this.resolve(lua)?.sink(id)))
        });

        methods.add_method("manage", |lua, this, id: WinID| {
            Ok(LuaWindowSet::materialised(this.resolve(lua)?.manage(id)))
        });

        methods.add_method("unmanage", |lua, this, id: WinID| {
            Ok(LuaWindowSet::materialised(this.resolve(lua)?.unmanage(id)))
        });

        methods.add_method("width", |lua, this, (id, ratio): (WinID, f64)| {
            Ok(LuaWindowSet::materialised(
                this.resolve(lua)?.width(id, ratio),
            ))
        });

        methods.add_method("stack", |lua, this, (id, onto): (WinID, WinID)| {
            Ok(LuaWindowSet::materialised(
                this.resolve(lua)?.stack(id, onto),
            ))
        });

        methods.add_method("tab", |lua, this, (id, onto): (WinID, WinID)| {
            Ok(LuaWindowSet::materialised(this.resolve(lua)?.tab(id, onto)))
        });

        methods.add_method("unstack", |lua, this, id: WinID| {
            Ok(LuaWindowSet::materialised(this.resolve(lua)?.unstack(id)))
        });
    }
}

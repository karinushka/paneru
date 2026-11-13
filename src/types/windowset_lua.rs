//! The [`WindowSet`] as a script sees it: its `UserData` impl and the
//! marshalling around it.
//!
//! `WindowSet` itself is the userdata — it is already pure and
//! immutable-by-transform, so no wrapper type is needed. Lives here rather
//! than in the Lua crate because of the orphan rule (`UserData` is mlua's,
//! `WindowSet` is ours). Gated behind the `lua` feature so a client wanting
//! only the wire types does not pull in an interpreter.
//!
//! # Indices
//!
//! Lua counts from one. Column indices and workspace numbers here do too, and
//! are translated at this boundary rather than anywhere deeper.

use mlua::{Function, LuaSerdeExt, UserData, UserDataMethods, Value};

use crate::types::windowset::{DisplaySet, LayoutOp, RelativeRect, WinID, WindowSet};

fn display_table(lua: &mlua::Lua, display: &DisplaySet) -> mlua::Result<mlua::Table> {
    let table = lua.create_table()?;
    table.set("id", display.id)?;
    table.set("active", display.active)?;
    table.set("x", display.frame.x)?;
    table.set("y", display.frame.y)?;
    table.set("width", display.frame.width)?;
    table.set("height", display.frame.height)?;
    Ok(table)
}

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

/// Reads what a `paneru.windows` transform handed back: the operations recorded
/// onto the window set it returned, or none at all if it returned nothing.
///
/// Shared so both hosts accept and reject exactly the same return values.
///
/// # Errors
///
/// Returns an error if the transform returned something other than a window set
/// or nil, or if the userdata it returned is already borrowed.
pub fn returned_ops(returned: &Value) -> mlua::Result<Vec<LayoutOp>> {
    match returned {
        Value::Nil => Ok(Vec::new()),
        Value::UserData(data) => Ok(data.borrow::<WindowSet>()?.ops()),
        other => Err(mlua::Error::RuntimeError(format!(
            "paneru.windows: expected a window set back, got {}",
            other.type_name()
        ))),
    }
}

impl UserData for WindowSet {
    #[allow(clippy::too_many_lines)]
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // --- reading ---------------------------------------------------

        methods.add_method("focused", |_, this, ()| Ok(this.focused()));

        methods.add_method("windows", |lua, this, ()| {
            this.windows()
                .map(|window| lua.to_value(window))
                .collect::<mlua::Result<Vec<Value>>>()
        });

        methods.add_method("window", |lua, this, id: WinID| match this.window(id) {
            Some(window) => lua.to_value(window),
            None => Ok(Value::Nil),
        });

        // The active workspace of the active display — "here", for a script.
        methods.add_method("current", |_, this, ()| {
            Ok(this.current().map(|workspace| workspace.number))
        });

        methods.add_method("workspaces", |_, this, ()| {
            Ok(this
                .workspaces()
                .map(|workspace| workspace.number)
                .collect::<Vec<u32>>())
        });

        // Every window on a workspace, tiled first then floating.
        methods.add_method("workspace_windows", |lua, this, number: u32| {
            let Some(workspace) = this.workspace(number) else {
                return Ok(Vec::new());
            };
            workspace
                .windows()
                .map(|window| lua.to_value(window))
                .collect::<mlua::Result<Vec<Value>>>()
        });

        // The workspace's columns, each a list of window ids, left to right.
        methods.add_method("columns", |_, this, number: Option<u32>| {
            let workspace = match number {
                Some(number) => this.workspace(number),
                None => this.current(),
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
        methods.add_method("column_of", |_, this, id: WinID| {
            Ok(this.column_of(id).map(|index| index + 1))
        });

        methods.add_method("workspace_of", |_, this, id: WinID| {
            Ok(this.workspace_of(id).map(|workspace| workspace.number))
        });

        // The whole display record, so a script can work out pixel geometry
        // itself when fractions are not enough.
        methods.add_method("display_of", |lua, this, id: WinID| {
            this.display_of(id).map_or(Ok(Value::Nil), |display| {
                display_table(lua, display).map(Value::Table)
            })
        });

        // Unlike `display_of`, this includes a display with no windows.
        methods.add_method("displays", |lua, this, ()| {
            this.displays()
                .iter()
                .map(|display| display_table(lua, display))
                .collect::<mlua::Result<Vec<_>>>()
        });

        methods.add_method("east", |_, this, id: WinID| Ok(this.east(id)));
        methods.add_method("west", |_, this, id: WinID| Ok(this.west(id)));
        methods.add_method("next", |_, this, id: WinID| Ok(this.next(id)));
        methods.add_method("prev", |_, this, id: WinID| Ok(this.prev(id)));

        // Predicates are plain Lua functions taking a window record;
        // `paneru.match` is a convenience, not a requirement.
        methods.add_method("find", |lua, this, predicate: Function| {
            for window in this.windows() {
                let record = lua.to_value(window)?;
                if predicate.call::<bool>(record.clone())? {
                    return Ok(record);
                }
            }
            Ok(Value::Nil)
        });

        methods.add_method("filter", |lua, this, predicate: Function| {
            let mut matched = Vec::new();
            for window in this.windows() {
                let record = lua.to_value(window)?;
                if predicate.call::<bool>(record.clone())? {
                    matched.push(record);
                }
            }
            Ok(matched)
        });

        // --- transforming ----------------------------------------------
        //
        // Each returns a *new* window set; the one it was called on is
        // untouched, and nothing reaches a real window until a handler
        // returns a set carrying these operations.

        methods.add_method("focus", |_, this, id: WinID| Ok(this.focus(id)));

        methods.add_method("swap", |_, this, (first, second): (WinID, WinID)| {
            Ok(this.swap(first, second))
        });

        methods.add_method(
            "shift",
            |_, this, (id, workspace, follow): (WinID, u32, Option<bool>)| {
                Ok(this.shift_following(id, workspace, follow.unwrap_or(false)))
            },
        );

        methods.add_method("view", |_, this, workspace: u32| Ok(this.view(workspace)));

        // `ws:float(id)` leaves the window where it is (defaultFloating);
        // `ws:float(id, rect)` places it (customFloating). The rect is given as
        // fractions of the display, like xmonad's RationalRect.
        methods.add_method(
            "float",
            |_, this, (id, rect): (WinID, Option<mlua::Table>)| {
                let Some(rect) = rect else {
                    return Ok(this.float(id));
                };
                Ok(this.float_at(id, relative_rect(&rect)?))
            },
        );

        methods.add_method("sink", |_, this, id: WinID| Ok(this.sink(id)));

        methods.add_method("manage", |_, this, id: WinID| Ok(this.manage(id)));

        methods.add_method("unmanage", |_, this, id: WinID| Ok(this.unmanage(id)));

        methods.add_method("width", |_, this, (id, ratio): (WinID, f64)| {
            Ok(this.width(id, ratio))
        });

        methods.add_method("stack", |_, this, (id, onto): (WinID, WinID)| {
            Ok(this.stack(id, onto))
        });

        methods.add_method("tab", |_, this, (id, onto): (WinID, WinID)| {
            Ok(this.tab(id, onto))
        });

        methods.add_method("unstack", |_, this, id: WinID| Ok(this.unstack(id)));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::types::state::Frame;
    use crate::types::windowset::{DisplaySet, WindowSet};

    fn empty_display(id: u32, x: i32) -> DisplaySet {
        DisplaySet {
            id,
            frame: Frame {
                x,
                y: 0,
                width: 100,
                height: 50,
            },
            active: id == 1,
            workspaces: Arc::new(Vec::new()),
        }
    }

    #[test]
    fn displays_lists_every_display_including_empty_ones() {
        let lua = mlua::Lua::new();
        let set = WindowSet::new(vec![empty_display(1, 0), empty_display(3, 100)], None);
        lua.globals().set("ws", set).expect("set the window set");
        let (ids, second_x, first_active): (Vec<u32>, i32, bool) = lua
            .load(
                "local displays = ws:displays()
                  local ids = {}
                  for index, display in ipairs(displays) do ids[index] = display.id end
                  return ids, displays[2].x, displays[1].active",
            )
            .eval()
            .expect("run the script");
        assert_eq!(ids, vec![1, 3]);
        assert_eq!(second_x, 100);
        assert!(first_active);
    }
}
